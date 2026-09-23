//! 网页面板后端（P20.1）——右侧「网页」面板：多标签管理 + 截图渲染 + 坐标回传。
//!
//! 与 agent 的 browser_* 工具共享同一无头浏览器（多标签互不干扰）；面板不走审批门
//! （用户亲自操作，非模型调用），但照受全局来源校验中间件保护（仅本机回环可访问）。

use super::*;
use axum::response::IntoResponse;

/// GET /api/browser/state —— 浏览器是否在跑 + 全部标签页（id/标题/地址/可见/加载/挂起）。
pub(super) async fn browser_state() -> Json<Value> {
    Json(crate::browser::panel_state().await)
}

/// POST /api/browser/new —— 开新标签页（body 可带 url，缺省 about:blank）。
pub(super) async fn browser_new(Json(req): Json<Value>) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let url = req.get("url").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty());
    match crate::browser::panel_new(url.map(str::to_string)).await {
        Ok(v) => Ok(Json(v)),
        Err(e) => Err(bad(e)),
    }
}

fn bad(e: String) -> (StatusCode, Json<Value>) {
    (StatusCode::BAD_REQUEST, Json(json!({ "error": e })))
}

/// POST /api/browser/navigate —— 地址栏回车（id + url）。
pub(super) async fn browser_navigate(Json(req): Json<Value>) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Some(id) = req.get("id").and_then(Value::as_str) else { return Err(bad("缺少 id".into())) };
    let Some(url) = req.get("url").and_then(Value::as_str) else { return Err(bad("缺少 url".into())) };
    match crate::browser::panel_navigate(id, url).await {
        Ok(v) => Ok(Json(v)),
        Err(e) => Err(bad(e)),
    }
}

/// GET /api/browser/screenshot?id= —— 当前视口 PNG（前端 <img> 直链，动作后换参数刷新）。
pub(super) async fn browser_screenshot(Query(q): Query<ScreenshotQuery>) -> Result<impl IntoResponse, (StatusCode, Json<Value>)> {
    match crate::browser::panel_screenshot(&q.id).await {
        Ok(png) => Ok((
            StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "image/png")],
            png,
        )),
        Err(e) => Err(bad(e)),
    }
}

#[derive(Debug, Deserialize)]
pub struct ScreenshotQuery {
    pub id: String,
}

/// GET /api/browser/frame?id= —— 面板帧（P21）：screencast 最新 JPEG，内存直出零 CDP
/// 往返；无帧时后端回落 captureScreenshot（PNG）。前端 <img> 轮询源。
pub(super) async fn browser_frame(Query(q): Query<ScreenshotQuery>) -> Result<impl IntoResponse, (StatusCode, Json<Value>)> {
    match crate::browser::panel_frame(&q.id).await {
        Ok((bytes, mime)) => Ok((
            StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, mime)],
            bytes,
        )),
        Err(e) => Err(bad(e)),
    }
}

/// POST /api/browser/activate —— 面板上报可见标签页（驱动 screencast 与后台挂起）。
pub(super) async fn browser_activate(Json(req): Json<Value>) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Some(id) = req.get("id").and_then(Value::as_str) else { return Err(bad("缺少 id".into())) };
    crate::browser::panel_activate(id).await.map(Json).map_err(bad)
}

/// POST /api/browser/dialog —— 面板处理页面 JS 弹窗（accept + 可选 promptText）。
pub(super) async fn browser_dialog(Json(req): Json<Value>) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Some(id) = req.get("id").and_then(Value::as_str) else { return Err(bad("缺少 id".into())) };
    let accept = req.get("accept").and_then(Value::as_bool).unwrap_or(true);
    let prompt_text = req.get("promptText").and_then(Value::as_str).map(str::to_string);
    crate::browser::panel_dialog(id, accept, prompt_text).await.map(Json).map_err(bad)
}

async fn click_impl(id: &str, req: &Value) -> Result<Value, String> {
    let x = req.get("x").and_then(Value::as_i64).unwrap_or(0);
    let y = req.get("y").and_then(Value::as_i64).unwrap_or(0);
    crate::browser::panel_click(id, x, y).await
}

/// POST /api/browser/click
pub(super) async fn browser_click(Json(req): Json<Value>) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Some(id) = req.get("id").and_then(Value::as_str) else { return Err(bad("缺少 id".into())) };
    click_impl(id, &req).await.map(Json).map_err(bad)
}

async fn type_impl(id: &str, req: &Value) -> Result<Value, String> {
    let text = req.get("text").and_then(Value::as_str).unwrap_or("");
    crate::browser::panel_type(id, text).await
}

/// POST /api/browser/type
pub(super) async fn browser_type(Json(req): Json<Value>) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Some(id) = req.get("id").and_then(Value::as_str) else { return Err(bad("缺少 id".into())) };
    type_impl(id, &req).await.map(Json).map_err(bad)
}

async fn key_impl(id: &str, req: &Value) -> Result<Value, String> {
    let key = req.get("key").and_then(Value::as_str).unwrap_or("Enter");
    let code = req.get("code").and_then(Value::as_i64).unwrap_or(13);
    crate::browser::panel_key(id, key, code).await
}

/// POST /api/browser/key
pub(super) async fn browser_key(Json(req): Json<Value>) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Some(id) = req.get("id").and_then(Value::as_str) else { return Err(bad("缺少 id".into())) };
    key_impl(id, &req).await.map(Json).map_err(bad)
}

async fn scroll_impl(id: &str, req: &Value) -> Result<Value, String> {
    let dy = req.get("deltaY").and_then(Value::as_i64).unwrap_or(0);
    crate::browser::panel_scroll(id, dy).await
}

/// POST /api/browser/scroll
pub(super) async fn browser_scroll(Json(req): Json<Value>) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Some(id) = req.get("id").and_then(Value::as_str) else { return Err(bad("缺少 id".into())) };
    scroll_impl(id, &req).await.map(Json).map_err(bad)
}

/// POST /api/browser/back
pub(super) async fn browser_back(Json(req): Json<Value>) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Some(id) = req.get("id").and_then(Value::as_str) else { return Err(bad("缺少 id".into())) };
    crate::browser::panel_back(id).await.map(Json).map_err(bad)
}

/// POST /api/browser/forward
pub(super) async fn browser_forward(Json(req): Json<Value>) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Some(id) = req.get("id").and_then(Value::as_str) else { return Err(bad("缺少 id".into())) };
    crate::browser::panel_forward(id).await.map(Json).map_err(bad)
}

/// POST /api/browser/reload
pub(super) async fn browser_reload(Json(req): Json<Value>) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Some(id) = req.get("id").and_then(Value::as_str) else { return Err(bad("缺少 id".into())) };
    crate::browser::panel_reload(id).await.map(Json).map_err(bad)
}

/// POST /api/browser/viewport —— 设置标签页视口（截图尺寸跟随面板大小）。
/// x/y = 面板内容区在壳窗口里的左上角（P22 shell 模式把 child webview 摆过去）。
pub(super) async fn browser_viewport(Json(req): Json<Value>) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Some(id) = req.get("id").and_then(Value::as_str) else { return Err(bad("缺少 id".into())) };
    let x = req.get("x").and_then(Value::as_i64).unwrap_or(0);
    let y = req.get("y").and_then(Value::as_i64).unwrap_or(0);
    let w = req.get("w").and_then(Value::as_i64).unwrap_or(1280);
    let h = req.get("h").and_then(Value::as_i64).unwrap_or(900);
    crate::browser::panel_viewport(id, x, y, w, h).await.map(Json).map_err(bad)
}

/// POST /api/browser/overlay —— 面板浮层（「⋯」菜单）开合。shell 模式开浮层时
/// 后端把 child webview 顶部让出 reserve（菜单实测高度，网页缩短一条、不被遮），
/// reserve≤0 退化为整页隐藏；关浮层恢复完整矩形。
pub(super) async fn browser_overlay(Json(req): Json<Value>) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let open = req.get("open").and_then(Value::as_bool).unwrap_or(false);
    let reserve = req.get("reserve").and_then(Value::as_i64).unwrap_or(0);
    crate::browser::panel_overlay(open, reserve).await.map(Json).map_err(bad)
}

/// POST /api/browser/open-external —— 在系统默认浏览器打开当前页。
pub(super) async fn browser_open_external(Json(req): Json<Value>) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Some(id) = req.get("id").and_then(Value::as_str) else { return Err(bad("缺少 id".into())) };
    crate::browser::panel_open_external(id).await.map(Json).map_err(bad)
}

/// POST /api/browser/devtools —— shell 模式指挥壳开真 DevTools；无头模式返回提示。
pub(super) async fn browser_devtools(Json(req): Json<Value>) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Some(id) = req.get("id").and_then(Value::as_str) else { return Err(bad("缺少 id".into())) };
    crate::browser::panel_devtools_url(id).await.map(Json).map_err(bad)
}

/// DELETE /api/browser/tab?id= —— 关标签页。
pub(super) async fn browser_close_tab(Query(q): Query<ScreenshotQuery>) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    match crate::browser::panel_close_tab(&q.id).await {
        Ok(v) => Ok(Json(v)),
        Err(e) => Err(bad(e)),
    }
}
