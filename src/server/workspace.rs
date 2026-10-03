//! 工作区切换与文件面板端点（files/file/changes/shell/picker）（P19 自 server.rs 按域拆出）

use super::*;

/// GET /api/workspace/files —— 工作区文件相对路径列表（「@ 添加上下文」选择器数据源；有界遍历）
pub(super) async fn list_workspace_files() -> Json<Value> {
    const MAX_FILES: usize = 500;
    const SKIP_DIRS: [&str; 9] = [
        ".git",
        ".agent",
        ".attachments",
        ".workbuddy",
        "node_modules",
        "target",
        "dist",
        "logs",
        "__pycache__",
    ];
    let ws = crate::tools::workspace_root();
    let mut files: Vec<String> = Vec::new();
    let mut stack: Vec<(std::path::PathBuf, String)> = vec![(ws.clone(), String::new())];
    while let Some((dir, rel)) = stack.pop() {
        if files.len() >= MAX_FILES {
            break;
        }
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut entries: Vec<_> = rd.filter_map(|e| e.ok()).collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            if files.len() >= MAX_FILES {
                break;
            }
            let name = e.file_name().to_string_lossy().to_string();
            let child_rel = if rel.is_empty() {
                name.clone()
            } else {
                format!("{rel}/{name}")
            };
            let p = e.path();
            if p.is_dir() {
                // 深度上限：防目录联接（junction）成环导致死循环
                if SKIP_DIRS.contains(&name.as_str()) || rel.matches('/').count() >= 12 {
                    continue;
                }
                stack.push((p, child_rel));
            } else {
                files.push(child_rel);
            }
        }
    }
    files.sort();
    let truncated = files.len() >= MAX_FILES;
    Json(json!({ "files": files, "truncated": truncated }))
}

/// GET /api/workspace/file?path=xx —— 读工作区文本文件内容（「预览」标签数据源；沿用 read_file 的越界防护与截断）
pub(super) async fn read_workspace_file(
    State(state): State<AppState>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let err = |e: String| (StatusCode::BAD_REQUEST, Json(json!({ "error": e })));
    let path = params.get("path").map(|s| s.trim()).unwrap_or("");
    if path.is_empty() {
        return Err(err("缺少 path".into()));
    }
    let sandbox_cfg = (*state.sandbox_cfg).clone();
    let cpu_percent = state.settings.read().unwrap_or_else(|e| e.into_inner()).sandbox_cpu_percent.unwrap_or(crate::sandbox::DEFAULT_SANDBOX_CPU_PERCENT);
    let exec = crate::tools::ExecConfig {
        max_chars: 200_000,
        sandbox: sandbox_cfg,
        unsandboxed: false,
        fs_unconfined: false,
        cpu_percent,
        workspace: crate::tools::workspace_root(),
    };
    match crate::tools::execute(
        &exec,
        "read_file",
        &json!({ "path": path }).to_string(),
    )
    .await
    {
        Ok(content) => Ok(Json(json!({ "ok": true, "content": content }))),
        Err(e) => Err(err(e)),
    }
}

/// GET /api/workspace/dir?path=xx —— 列工作区某个目录的**直接子项**（「项目文件」树面板数据源）：
/// 折叠/展开按需拉一层，不把整棵树铺开。path 缺省/空 = 工作区根。
/// 跳过 SKIP_DIRS 那批噪音目录；每类最多 500 项，超了带 truncated。
pub(super) async fn list_workspace_dir(
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    const MAX_ENTRIES: usize = 500;
    const SKIP_DIRS: [&str; 9] = [
        ".git",
        ".agent",
        ".attachments",
        ".workbuddy",
        "node_modules",
        "target",
        "dist",
        "logs",
        "__pycache__",
    ];
    let rel = params.get("path").map(|s| s.trim()).unwrap_or("").to_string();
    let res = tokio::task::spawn_blocking(move || -> Result<Value, String> {
        let dir = crate::tools::resolve_workspace_path(&rel)?;
        if !dir.is_dir() {
            return Err(format!("不是目录: {rel}"));
        }
        let rd = std::fs::read_dir(&dir).map_err(|e| format!("读取目录失败: {e}"))?;
        let mut dirs: Vec<String> = Vec::new();
        let mut files: Vec<String> = Vec::new();
        let mut truncated = false;
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
            if is_dir {
                if SKIP_DIRS.contains(&name.as_str()) {
                    continue;
                }
                if dirs.len() >= MAX_ENTRIES {
                    truncated = true;
                    continue;
                }
                dirs.push(name);
            } else {
                if files.len() >= MAX_ENTRIES {
                    truncated = true;
                    continue;
                }
                files.push(name);
            }
        }
        dirs.sort();
        files.sort();
        Ok(json!({ "ok": true, "dirs": dirs, "files": files, "truncated": truncated }))
    })
    .await;
    match res {
        Ok(Ok(v)) => Ok(Json(v)),
        Ok(Err(e)) => Err((StatusCode::BAD_REQUEST, Json(json!({ "error": e })))),
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() })))),
    }
}

/// GET /api/workspace/raw?path=xx —— 原样直出工作区文件字节（图片预览用）。
/// 只给浏览器渲染，不解析成文本；工作区边界与 read_file 同一套硬拦，单文件上限 8MB。
pub(super) async fn read_workspace_raw(
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Result<axum::response::Response, (StatusCode, Json<Value>)> {
    use axum::response::IntoResponse;
    const MAX_BYTES: u64 = 8 * 1024 * 1024;
    let path = params.get("path").map(|s| s.trim()).unwrap_or("").to_string();
    if path.is_empty() {
        return Err((StatusCode::BAD_REQUEST, Json(json!({ "error": "缺少 path" }))));
    }
    let res = tokio::task::spawn_blocking(move || -> Result<(Vec<u8>, String), String> {
        let full = crate::tools::resolve_workspace_path(&path)?;
        let meta = std::fs::metadata(&full).map_err(|e| format!("读取失败: {e}"))?;
        if !meta.is_file() {
            return Err(format!("不是文件: {path}"));
        }
        if meta.len() > MAX_BYTES {
            return Err(format!("文件过大（{} 字节，上限 8MB）", meta.len()));
        }
        let bytes = std::fs::read(&full).map_err(|e| format!("读取失败: {e}"))?;
        let ext = full.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
        let ct = match ext.as_str() {
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "gif" => "image/gif",
            "webp" => "image/webp",
            "svg" => "image/svg+xml",
            "bmp" => "image/bmp",
            "ico" => "image/x-icon",
            "avif" => "image/avif",
            // 文档与媒体直出（预览面板内嵌渲染：PDF 走浏览器自带查看器、音视频走原生标签）
            "pdf" => "application/pdf",
            "mp4" | "m4v" => "video/mp4",
            "webm" => "video/webm",
            "mov" => "video/quicktime",
            "mp3" => "audio/mpeg",
            "wav" => "audio/wav",
            "ogg" => "audio/ogg",
            "m4a" => "audio/mp4",
            "flac" => "audio/flac",
            _ => "application/octet-stream",
        };
        Ok((bytes, ct.to_string()))
    })
    .await;
    match res {
        Ok(Ok((bytes, ct))) => Ok((
            [(axum::http::header::CONTENT_TYPE, ct), (axum::http::header::CACHE_CONTROL, "no-store".to_string())],
            bytes,
        )
            .into_response()),
        Ok(Err(e)) => Err((StatusCode::BAD_REQUEST, Json(json!({ "error": e })))),
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() })))),
    }
}

/// GET /api/workspace/doc-slides?path=xx —— pptx 结构化分页（预览面板逐页渲染用）。
pub(super) async fn read_workspace_doc_slides(
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let path = params.get("path").map(|s| s.trim()).unwrap_or("").to_string();
    if path.is_empty() {
        return Err((StatusCode::BAD_REQUEST, Json(json!({ "error": "缺少 path" }))));
    }
    let res = tokio::task::spawn_blocking(move || -> Result<String, String> {
        let full = crate::tools::resolve_workspace_path(&path).map_err(|e| e.to_string())?;
        let ext = full.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
        if ext != "pptx" {
            return Err(format!("只支持 pptx（这是 .{ext}）"));
        }
        crate::doctext::pptx_slides_json(&full).map_err(|e| e.to_string())
    })
    .await;
    match res {
        Ok(Ok(slides)) => {
            let v: Value = serde_json::from_str(&slides).unwrap_or_else(|_| json!([]));
            Ok(Json(json!({ "ok": true, "slides": v })))
        }
        Ok(Err(e)) => Err((StatusCode::BAD_REQUEST, Json(json!({ "error": e })))),
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() })))),
    }
}

#[derive(Deserialize)]
pub struct ShellReq {
    pub command: String,
}

/// GET /api/workspace/doc?path=xx —— Office 文档文本抽取（预览面板用）。
/// docx/pptx/xlsx/odt/ods/odp → 可读文本（doctext.rs，best-effort 去标签）；
/// 工作区边界与 raw 同一套硬拦，单文件上限 50MB（文档容器可以很大）。
pub(super) async fn read_workspace_doc(
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    const MAX_BYTES: u64 = 50 * 1024 * 1024;
    let path = params.get("path").map(|s| s.trim()).unwrap_or("").to_string();
    if path.is_empty() {
        return Err((StatusCode::BAD_REQUEST, Json(json!({ "error": "缺少 path" }))));
    }
    let res = tokio::task::spawn_blocking(move || -> Result<String, String> {
        let full = crate::tools::resolve_workspace_path(&path).map_err(|e| e.to_string())?;
        let meta = std::fs::metadata(&full).map_err(|e| format!("读取失败: {e}"))?;
        if !meta.is_file() {
            return Err(format!("不是文件: {path}"));
        }
        if meta.len() > MAX_BYTES {
            return Err(format!("文件过大（{} 字节，上限 50MB）", meta.len()));
        }
        crate::doctext::extract(&full).map_err(|e| e.to_string())
    })
    .await;
    match res {
        Ok(Ok(text)) => Ok(Json(json!({ "ok": true, "text": text }))),
        Ok(Err(e)) => Err((StatusCode::BAD_REQUEST, Json(json!({ "error": e })))),
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() })))),
    }
}

/// POST /api/shell —— 终端面板：在沙箱内执行一条用户输入的命令（复用 shell 工具的沙箱与超时）
pub(super) async fn run_shell(
    State(state): State<AppState>,
    Json(req): Json<ShellReq>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let command = req.command.trim().to_string();
    if command.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "命令为空" })),
        ));
    }
    let sandbox_cfg = (*state.sandbox_cfg).clone();
    let cpu_percent = state.settings.read().unwrap_or_else(|e| e.into_inner()).sandbox_cpu_percent.unwrap_or(crate::sandbox::DEFAULT_SANDBOX_CPU_PERCENT);
    let exec = crate::tools::ExecConfig {
        max_chars: 100_000,
        sandbox: sandbox_cfg,
        unsandboxed: false,
        fs_unconfined: false,
        cpu_percent,
        workspace: crate::tools::workspace_root(),
    };
    match crate::tools::execute(&exec, "shell", &json!({ "command": command }).to_string()).await {
        Ok(output) => Ok(Json(json!({ "ok": true, "output": output }))),
        Err(e) => Ok(Json(json!({ "ok": false, "output": e }))),
    }
}

#[derive(Debug, Deserialize)]
pub struct WorkspaceRequest {
    pub path: String,
}

/// GET /api/workspace —— 当前工作区 + 最近列表（P8.2）。
pub(super) async fn get_workspace(State(state): State<AppState>) -> Json<Value> {
    let s = state.settings.read().unwrap_or_else(|e| e.into_inner());
    Json(json!({
        "current": crate::tools::workspace_root().display().to_string(),
        "recent": s.recent_workspaces,
    }))
}

/// 规范化工作区路径：Windows canonicalize 会加 `\\?\` 扩展前缀，显示与持久化都难看，剥掉。
pub(crate) fn canon_workspace(p: std::path::PathBuf) -> std::path::PathBuf {
    match p.canonicalize() {
        Ok(c) => {
            let s = c.display().to_string();
            match s.strip_prefix(r"\\?\") {
                Some(stripped) => std::path::PathBuf::from(stripped),
                None => c,
            }
        }
        Err(_) => p,
    }
}

/// 路径等价比较（Windows 大小写不敏感、斜杠方向与结尾分隔符归一）。
pub(super) fn same_workspace(a: &str, b: &str) -> bool {
    let norm = |s: &str| {
        s.replace('/', "\\")
            .trim_end_matches('\\')
            .to_lowercase()
    };
    norm(a) == norm(b)
}

/// 切换工作区（/api/workspace 与「打开会话」共用）：改全局根 → 记忆层热重开
/// → settings 持久化 + 最近列表 → MCP 同步 → 事件。调用方负责校验目录存在。
pub(crate) fn apply_workspace(state: &AppState, canon: &std::path::Path) -> Result<(), String> {
    let old = crate::tools::set_workspace_root(canon);
    // 记忆层重开：新项目的双层存储/index 登记/AGENTS.md 模板
    let new_mem = Arc::new(crate::memory::Memory::open(canon, &state.mem_cfg));
    new_mem.ensure_agents_template(canon);
    *state.memory.write().unwrap_or_else(|e| e.into_inner()) = new_mem;
    // 持久化 + 最近列表（去重、新→旧、最多 8）
    let cur = canon.display().to_string();
    let mut s = state.settings.write().unwrap_or_else(|e| e.into_inner()).clone();
    s.workspace = Some(cur.clone());
    s.recent_workspaces.retain(|w| *w != cur);
    s.recent_workspaces.insert(0, cur.clone());
    s.recent_workspaces.truncate(8);
    s.save().map_err(|e| e.to_string())?;
    *state.settings.write().unwrap_or_else(|e| e.into_inner()) = s.clone();
    // P18 存储一门/契约（027）：settings 变更即同步停用名单到工具插件（拒新调用）
    crate::plugin::wasm::set_disabled(&s.disabled_plugins);
    crate::mcp::sync_configs(&s.mcp_servers); // P11：MCP 配置变更即同步管理器（幂等）
    observability::event(
        EventType::State,
        json!({ "kind": "workspace.switched", "from": old.display().to_string(), "to": cur }),
    );
    Ok(())
}

/// POST /api/workspace —— 切换工作区：校验目录存在 → 改全局根 → 记忆层热重开
/// （新项目 .agent/index/AGENTS.md 模板）→ settings 持久化 + 最近列表。
/// 进行中的任务不拦（前端流式期间禁用按钮）；切换只影响之后的工具调用。
pub(super) async fn set_workspace(
    State(state): State<AppState>,
    Json(req): Json<WorkspaceRequest>,
) -> (StatusCode, Json<Value>) {
    let path = req.path.trim();
    if path.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "路径为空" })));
    }
    let p = std::path::PathBuf::from(path);
    if !p.is_dir() {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": format!("目录不存在: {path}") })));
    }
    let canon = canon_workspace(p);
    if let Err(e) = apply_workspace(&state, &canon) {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e })));
    }
    let recent = state.settings.read().unwrap_or_else(|e| e.into_inner()).recent_workspaces.clone();
    (StatusCode::OK, Json(json!({ "ok": true, "current": canon.display().to_string(), "recent": recent })))
}

/// POST /api/workspace/file —— UI 扩展写工作区文件（P17 组件总线写操作；宿主侧已按 manifest
/// 权限 + 用户确认把关）。边界与工具的 write_file 一致：工作区内、512KB、原子写。
pub(super) async fn write_workspace_file(Json(req): Json<Value>) -> (StatusCode, Json<Value>) {
    let path = req.get("path").and_then(Value::as_str).unwrap_or("").trim().to_string();
    let content = req.get("content").and_then(Value::as_str).unwrap_or("").to_string();
    if path.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "缺少 path" })));
    }
    if content.len() > 512 * 1024 {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "内容过大（上限 512KB）" })));
    }
    match crate::tools::write_workspace_relative(&path, &content) {
        Ok(bytes) => {
            observability::event(
                EventType::State,
                json!({ "kind": "ext.workspace_write", "path": path, "bytes": bytes }),
            );
            (StatusCode::OK, Json(json!({ "ok": true, "path": path, "bytes": bytes })))
        }
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))),
    }
}

/// POST /api/workspace/picker —— 弹 Win 原生选文件夹窗口（rfd/IFileDialog），返回所选路径。
/// 用户取消时 canceled=true。对话框由本地 server 进程弹出，即系统原生窗口。
pub(super) async fn pick_workspace() -> (StatusCode, Json<Value>) {
    let res = tokio::task::spawn_blocking(|| rfd::FileDialog::new().pick_folder()).await;
    match res {
        Ok(Some(path)) => {
            // Windows canonicalize 加 \\?\ 前缀难看；rfd 返回的是用户选择的原始路径，直接清洗
            let s = path.display().to_string();
            let s = s.strip_prefix(r"\\?\").unwrap_or(&s).to_string();
            (StatusCode::OK, Json(json!({ "ok": true, "path": s })))
        }
        Ok(None) => (StatusCode::OK, Json(json!({ "ok": true, "canceled": true }))),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))),
    }
}

/// POST /api/workspace/remove —— 从侧边栏项目列表（recent_workspaces）移除（不删目录）。
pub(super) async fn remove_workspace(
    State(state): State<AppState>,
    Json(req): Json<WorkspaceRequest>,
) -> (StatusCode, Json<Value>) {
    let path = req.path.trim();
    if path.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "路径为空" })));
    }
    let mut s = state.settings.write().unwrap_or_else(|e| e.into_inner()).clone();
    s.recent_workspaces.retain(|w| w != path);
    if let Err(e) = s.save() {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() })));
    }
    *state.settings.write().unwrap_or_else(|e| e.into_inner()) = s.clone();
    // P18 存储一门/契约（027）：settings 变更即同步停用名单到工具插件（拒新调用）
    crate::plugin::wasm::set_disabled(&s.disabled_plugins);
    crate::mcp::sync_configs(&s.mcp_servers); // P11：MCP 配置变更即同步管理器（幂等）
    observability::event(EventType::State, json!({ "kind": "workspace.removed", "path": path }));
    (StatusCode::OK, Json(json!({ "ok": true, "recent": s.recent_workspaces })))
}

/// POST /api/workspace/alias —— 侧边栏「重命名项目」：工作区显示别名；空名=清除别名。
pub(super) async fn alias_workspace(
    State(state): State<AppState>,
    Json(req): Json<WorkspaceAliasRequest>,
) -> (StatusCode, Json<Value>) {
    let path = req.path.trim();
    if path.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "路径为空" })));
    }
    let mut s = state.settings.write().unwrap_or_else(|e| e.into_inner()).clone();
    let name = req.name.trim().to_string();
    if name.is_empty() {
        s.workspace_aliases.remove(path);
    } else {
        s.workspace_aliases.insert(path.to_string(), name.clone());
    }
    if let Err(e) = s.save() {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() })));
    }
    *state.settings.write().unwrap_or_else(|e| e.into_inner()) = s.clone();
    // P18 存储一门/契约（027）：settings 变更即同步停用名单到工具插件（拒新调用）
    crate::plugin::wasm::set_disabled(&s.disabled_plugins);
    crate::mcp::sync_configs(&s.mcp_servers); // P11：MCP 配置变更即同步管理器（幂等）
    observability::event(
        EventType::State,
        json!({ "kind": "workspace.aliased", "path": path, "name": name }),
    );
    (StatusCode::OK, Json(json!({ "ok": true })))
}

#[derive(Debug, Deserialize)]
pub struct WorkspaceAliasRequest {
    pub path: String,
    pub name: String,
}
