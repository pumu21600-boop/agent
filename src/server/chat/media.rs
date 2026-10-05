use super::*;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaAttachment {
    pub name: String,
    pub mime: String,
    pub data: String,
}

impl MediaAttachment {
    pub fn is_audio(&self) -> bool {
        let n = self.name.to_lowercase();
        self.mime.starts_with("audio/")
            || n.ends_with(".mp3")
            || n.ends_with(".wav")
            || n.ends_with(".m4a")
            || n.ends_with(".ogg")
            || n.ends_with(".flac")
            || n.ends_with(".aac")
            || n.ends_with(".wma")
    }

    pub fn is_video(&self) -> bool {
        let n = self.name.to_lowercase();
        self.mime.starts_with("video/")
            || n.ends_with(".mp4")
            || n.ends_with(".webm")
            || n.ends_with(".mov")
            || n.ends_with(".avi")
            || n.ends_with(".mkv")
            || n.ends_with(".flv")
    }

    /// Office/PDF 类文档附件：不走多模态内联（视觉模型解析不了二进制文档），
    /// 一律落盘 + 文本通知交给文档处理技能。
    pub fn is_document(&self) -> bool {
        let n = self.name.to_lowercase();
        self.mime.starts_with("application/vnd.")
            || self.mime == "application/pdf"
            || self.mime == "application/rtf"
            || self.mime == "text/csv"
            || n.ends_with(".docx")
            || n.ends_with(".doc")
            || n.ends_with(".xlsx")
            || n.ends_with(".xls")
            || n.ends_with(".pptx")
            || n.ends_with(".ppt")
            || n.ends_with(".pdf")
            || n.ends_with(".odt")
            || n.ends_with(".ods")
            || n.ends_with(".odp")
            || n.ends_with(".rtf")
            || n.ends_with(".csv")
    }

    /// 文档类的人话类型名（用于通知文案）
    pub fn document_kind_cn(&self) -> &'static str {
        let n = self.name.to_lowercase();
        if n.ends_with(".docx") || n.ends_with(".doc") || n.ends_with(".odt") || n.ends_with(".rtf") {
            "Word 文档"
        } else if n.ends_with(".xlsx") || n.ends_with(".xls") || n.ends_with(".ods") || n.ends_with(".csv") {
            "Excel 表格"
        } else if n.ends_with(".pptx") || n.ends_with(".ppt") || n.ends_with(".odp") {
            "PowerPoint 演示"
        } else if n.ends_with(".pdf") {
            "PDF 文档"
        } else {
            "Office 文档"
        }
    }

    /// 对应的文档处理技能名（无对应技能的返回空串）
    pub fn document_skill(&self) -> &'static str {
        let n = self.name.to_lowercase();
        if n.ends_with(".docx") || n.ends_with(".odt") {
            "docx"
        } else if n.ends_with(".xlsx") || n.ends_with(".xls") || n.ends_with(".ods") || n.ends_with(".csv") {
            "xlsx"
        } else if n.ends_with(".pptx") || n.ends_with(".ppt") || n.ends_with(".odp") {
            "pptx"
        } else if n.ends_with(".pdf") {
            "pdf"
        } else {
            ""
        }
    }

    pub fn is_image(&self) -> bool {
        let n = self.name.to_lowercase();
        self.mime.starts_with("image/")
            || n.ends_with(".png")
            || n.ends_with(".jpg")
            || n.ends_with(".jpeg")
            || n.ends_with(".webp")
            || n.ends_with(".gif")
            || n.ends_with(".bmp")
            || n.ends_with(".svg")
            || (!self.is_audio() && !self.is_video() && !self.is_document())
    }

    /// 老格式（Office 97-2003 二进制 / ODF）→ 目标 OOXML 扩展名。
    /// 现有技能工具链只认 OOXML（zip+XML）：这些格式要么打不开、要么只能读不能改，
    /// 需先用 LibreOffice 转出副本再进流水线。
    pub fn legacy_target(&self) -> Option<&'static str> {
        let n = self.name.to_lowercase();
        if n.ends_with(".doc") || n.ends_with(".odt") {
            Some("docx")
        } else if n.ends_with(".xls") || n.ends_with(".ods") {
            Some("xlsx")
        } else if n.ends_with(".ppt") || n.ends_with(".odp") {
            Some("pptx")
        } else {
            None
        }
    }

    /// 收件链路入口：附件已落盘 `.attachments/<rel_name>`，产出注入消息的通知文本。
    /// 普通格式原样出通知；老格式先尝试 LibreOffice 转成 OOXML 副本（装了才用）——
    /// 转成了通知指向可加工的副本（原件不动），转不了按真实能力如实说明
    /// （表格老格式能读不能改；二进制/ODF 办不了，ODF 可预览）。不许指歪路。
    pub async fn document_intake_notice(&self, ws: &Path, rel_name: &str) -> String {
        let bin = find_soffice();
        self.document_intake_notice_with(ws, rel_name, bin.as_deref())
            .await
    }

    /// 可注入转换器的内部实现（测试用固定 None/假路径，避免依赖机器环境）。
    async fn document_intake_notice_with(
        &self,
        ws: &Path,
        rel_name: &str,
        soffice: Option<&Path>,
    ) -> String {
        let Some(ext) = self.legacy_target() else {
            return self.document_notice(rel_name);
        };
        match soffice {
            Some(bin) => {
                let att_dir = ws.join(".attachments");
                match convert_legacy_file(bin, &att_dir, rel_name, ext).await {
                    Ok(converted_rel) => self.converted_notice(rel_name, &converted_rel),
                    Err(e) => self
                        .legacy_unavailable_notice(rel_name, &format!("LibreOffice 转换失败（{e}）")),
                }
            }
            None => self.legacy_unavailable_notice(rel_name, "本机没有找到可用的 LibreOffice"),
        }
    }

    /// 转换成功：通知指向可加工的 OOXML 副本；交付名按原文件词干建议
    /// （合同.doc → 交付 合同_updated.docx），原件保留供对照。
    fn converted_notice(&self, orig_rel: &str, converted_rel: &str) -> String {
        let conv = MediaAttachment {
            name: converted_rel.to_string(),
            mime: String::new(),
            data: String::new(),
        };
        let stem = Path::new(&self.name)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("output");
        let body = conv.notice_with_delivery(converted_rel, Some(stem));
        format!(
            "[📄 老格式文档已自动转换：「{}」（{}）是旧版格式（Office 97-2003 二进制 / ODF），现有技能引擎打不开，已用 LibreOffice 转出可加工副本：.attachments/{converted_rel}（原件 .attachments/{orig_rel} 未改动，仅供对照）。以下按转换后的副本处理：\n{body}]",
            self.name,
            self.document_kind_cn()
        )
    }

    /// 老格式但本机转不了（无转换器/转换失败）：按真实能力如实说明，
    /// 并给用户可执行的出路；不许二进制硬读、另造脚本或假装完成。
    fn legacy_unavailable_notice(&self, rel_name: &str, reason: &str) -> String {
        let n = self.name.to_lowercase();
        let kind = self.document_kind_cn();
        if n.ends_with(".xls") || n.ends_with(".ods") {
            format!(
                "[📄 文档附件已收到：「{}」（{kind}），已保存至工作区：.attachments/{rel_name}\n这是 Excel 老格式：{reason}，无法自动转成 .xlsx，所以「能读不能改」（无法保真修改）——\n1. 用户只是要查/汇总内容：先 read_skill(\"xlsx\")，按技能 read 命令直接读出（引擎支持 .xls/.ods）；\n2. 用户要修改：如实说明保真修改需要 .xlsx（请用 Excel/WPS 另存为 .xlsx 后再发）；若数据简单且用户接受，可读出内容用 make 重建新 .xlsx，但必须讲清「原样式/公式等格式不保真」。不要二进制硬改，不要假装完成。]",
                self.name
            )
        } else if n.ends_with(".odt") || n.ends_with(".odp") {
            format!(
                "[📄 文档附件已收到：「{}」（{kind}），已保存至工作区：.attachments/{rel_name}\n这是 ODF 格式：{reason}，无法自动转成可加工格式。用户可以先点预览看内容：[{}](/api/workspace/raw?path=.attachments/{rel_name})；要加工（修改/生成新文件）时如实说明：请用 LibreOffice/WPS 另存为 {} 后再发一次。不要二进制硬读，不要另造解析脚本，不要假装完成。]",
                self.name,
                self.name,
                if n.ends_with(".odt") { ".docx" } else { ".pptx" }
            )
        } else {
            format!(
                "[📄 文档附件已收到：「{}」（{kind}），已保存至工作区：.attachments/{rel_name}\n这是 Office 97-2003 老格式（二进制）：{reason}，无法自动转成可加工格式，现在处理不了。\n请如实告诉用户：用 Word/WPS 另存为 {} 后再发一次；不要二进制硬读，不要另造解析脚本，不要假装完成。]",
                self.name,
                if n.ends_with(".ppt") { ".pptx" } else { ".docx" }
            )
        }
    }

    /// 文档类附件的通知文案：合入最后一条 user 消息（随快照落盘，历史重开仍可见），
    /// 主模型据此走文档技能流水线而不是把二进制当图片内联。
    pub fn document_notice(&self, rel_name: &str) -> String {
        self.notice_with_delivery(rel_name, None)
    }

    /// 通知正文。deliver_stem：交付文件名的词干——转换副本场景传原文件词干，
    /// 让「默认交付 xxx_updated」贴着用户认识的名字。
    fn notice_with_delivery(&self, rel_name: &str, deliver_stem: Option<&str>) -> String {
        let skill = self.document_skill();
        let route = if skill.is_empty() {
            "这是纯文本可读的文档，直接用 read_file 读取即可".to_string()
        } else {
            format!("先 read_skill(\"{skill}\") 读取对应技能，再按技能指引的命令处理（禁止裸解 zip、禁止另造脚本、本机没有 Python）")
        };
        let stem = deliver_stem.unwrap_or_else(|| {
            Path::new(&self.name)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("output")
        });
        let deliver = match Path::new(&self.name).extension().and_then(|s| s.to_str()) {
            Some(e) => format!("默认交付 {stem}_updated.{e} 副本"),
            None => format!("默认交付 {stem}_updated 副本"),
        };
        format!(
            "[📄 文档附件已收到：「{}」（{}），已保存至工作区：.attachments/{}\n这是用户直接发来的文档文件。处理要求：\n1. {}；\n2. 用户的具体诉求就在本条消息正文里，先明确诉求再动手；拿不准能力边界时如实说明，不要假装完成；\n3. 改完把结果文件路径明确报给用户；修改类任务{}，用户明说「原地改/覆盖原文件」才写原路径；\n4. 需要用户查看结果文件时，用 markdown 链接给出，前端会渲染成可点击跳预览：[原文件名.docx](/api/workspace/raw?path=.attachments/{})]",
            self.name,
            self.document_kind_cn(),
            rel_name,
            route,
            deliver,
            rel_name
        )
    }

    pub fn audio_format(&self) -> String {
        let n = self.name.to_lowercase();
        if n.ends_with(".wav") || self.mime.contains("wav") {
            "wav".into()
        } else if n.ends_with(".ogg") || self.mime.contains("ogg") {
            "ogg".into()
        } else if n.ends_with(".m4a") || self.mime.contains("m4a") {
            "m4a".into()
        } else if n.ends_with(".flac") || self.mime.contains("flac") {
            "flac".into()
        } else if n.ends_with(".aac") || self.mime.contains("aac") {
            "aac".into()
        } else {
            "mp3".into()
        }
    }

    pub fn raw_base64(&self) -> String {
        if let Some(idx) = self.data.find(";base64,") {
            self.data[idx + 8..].to_string()
        } else if self.data.starts_with("data:") {
            if let Some(idx) = self.data.find(',') {
                self.data[idx + 1..].to_string()
            } else {
                self.data.clone()
            }
        } else {
            self.data.clone()
        }
    }

    pub fn data_url(&self) -> String {
        if self.data.starts_with("data:") {
            self.data.clone()
        } else {
            format!("data:{};base64,{}", self.effective_mime(), self.data)
        }
    }

    pub fn effective_mime(&self) -> String {
        if !self.mime.is_empty() && self.mime != "application/octet-stream" {
            return self.mime.clone();
        }
        let n = self.name.to_lowercase();
        if n.ends_with(".mp3") { "audio/mp3".into() }
        else if n.ends_with(".wav") { "audio/wav".into() }
        else if n.ends_with(".m4a") { "audio/m4a".into() }
        else if n.ends_with(".ogg") { "audio/ogg".into() }
        else if n.ends_with(".mp4") { "video/mp4".into() }
        else if n.ends_with(".webm") { "video/webm".into() }
        else if n.ends_with(".mov") { "video/quicktime".into() }
        else if n.ends_with(".png") { "image/png".into() }
        else if n.ends_with(".jpg") || n.ends_with(".jpeg") { "image/jpeg".into() }
        else if n.ends_with(".webp") { "image/webp".into() }
        else { "application/octet-stream".into() }
    }
}


/// 找 LibreOffice（soffice）：环境变量 AGENT_SOFFICE_PATH / SOFFICE_PATH →
/// PATH 各目录 → Windows 常见安装路径。都没有返回 None——调用方走「如实说处理不了」
/// 的分支，不许静默降级。（与技能 check.js 的 SOFFICE_PATH 约定保持同一套）
pub fn find_soffice() -> Option<PathBuf> {
    for key in ["AGENT_SOFFICE_PATH", "SOFFICE_PATH"] {
        if let Some(v) = std::env::var_os(key) {
            let p = PathBuf::from(v);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    if let Some(path_var) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_var) {
            for exe in ["soffice.exe", "soffice.com", "soffice"] {
                let cand = dir.join(exe);
                if cand.is_file() {
                    return Some(cand);
                }
            }
        }
    }
    [
        r"C:\Program Files\LibreOffice\program\soffice.exe",
        r"C:\Program Files (x86)\LibreOffice\program\soffice.exe",
    ]
    .iter()
    .map(PathBuf::from)
    .find(|p| p.is_file())
}

/// 转换副本命名：`<原词干>_converted.<ext>`，重名再叠 `_2`（绝不覆盖已有文件）。
fn converted_target(att_dir: &Path, stem: &str, ext: &str) -> PathBuf {
    let first = att_dir.join(format!("{stem}_converted.{ext}"));
    if !first.exists() {
        return first;
    }
    for i in 2..100 {
        let cand = att_dir.join(format!("{stem}_converted_{i}.{ext}"));
        if !cand.exists() {
            return cand;
        }
    }
    first
}

/// 跑 soffice 把老格式转成 OOXML 副本，成功返回副本文件名（.attachments 内）。
/// 输出先进临时目录再改名：soffice 按输入词干命名，直接写 .attachments 可能覆盖用户同名文件。
/// 用独立 UserInstallation，避免和用户正在开着的 LibreOffice 抢单实例。
async fn convert_legacy_file(
    soffice: &Path,
    att_dir: &Path,
    rel_name: &str,
    ext: &str,
) -> Result<String, String> {
    let src = att_dir.join(rel_name);
    let stem = Path::new(rel_name)
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("attachment");
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = std::env::temp_dir().join(format!("agent-doc-convert-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&tmp).map_err(|e| format!("创建临时目录失败：{e}"))?;
    let profile = tmp.join("lo-profile");
    let profile_url = format!("file:///{}", profile.display()).replace('\\', "/").replace(' ', "%20");

    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        tokio::process::Command::new(soffice)
            .arg("--headless")
            .arg("--norestore")
            .arg(format!("-env:UserInstallation={profile_url}"))
            .arg("--convert-to")
            .arg(ext)
            .arg("--outdir")
            .arg(&tmp)
            .arg(&src)
            .kill_on_drop(true)
            .output(),
    )
    .await;
    let status: Result<(), String> = match outcome {
        Err(_) => Err("转换超时（120 秒）".to_string()),
        Ok(Err(e)) => Err(format!("转换器启动失败：{e}")),
        Ok(Ok(o)) => {
            if o.status.success() {
                Ok(())
            } else {
                let tail = String::from_utf8_lossy(&o.stderr);
                let last = tail.lines().last().unwrap_or("").trim();
                Err(format!("转换器退出码 {:?}：{last}", o.status.code()))
            }
        }
    };

    let result = match status {
        Ok(()) => {
            let produced = tmp.join(format!("{stem}.{ext}"));
            // Windows 上 soffice.exe 是壳，极端情况下退出可能早于产物落地：留一点等待
            let mut waited = 0;
            while !produced.is_file() && waited < 30 {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                waited += 1;
            }
            if produced.is_file() {
                let dest = converted_target(att_dir, stem, ext);
                std::fs::rename(&produced, &dest)
                    .or_else(|_| std::fs::copy(&produced, &dest).map(|_| ())) // 跨盘 rename 不行就复制
                    .map_err(|e| format!("写入转换副本失败：{e}"))
                    .and_then(|()| {
                        dest.file_name()
                            .and_then(|n| n.to_str())
                            .map(|s| s.to_string())
                            .ok_or_else(|| "转换副本命名失败".to_string())
                    })
            } else {
                Err("转换器退出但没有产出目标文件".to_string())
            }
        }
        Err(e) => Err(e),
    };
    let _ = std::fs::remove_dir_all(&tmp);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn att(name: &str, mime: &str) -> MediaAttachment {
        MediaAttachment {
            name: name.to_string(),
            mime: mime.to_string(),
            data: "data:application/octet-stream;base64,AAAA".to_string(),
        }
    }

    #[test]
    fn document_classification_by_extension_even_with_generic_mime() {
        // 拖拽进来的文件 mime 常是空/octet-stream，必须按扩展名判文档
        for n in [
            "a.docx", "a.xlsx", "a.pptx", "a.pdf", "a.doc", "a.xls", "a.ppt", "a.odt", "a.ods",
            "a.odp", "a.csv", "a.rtf", "A.DOCX", "A.Pdf",
        ] {
            let a = att(n, "");
            assert!(a.is_document(), "{n} 应判为文档");
            assert!(!a.is_image(), "{n} 不应再落进图片兜底（视觉模型解析不了二进制文档）");
            assert!(!a.is_audio() && !a.is_video(), "{n} 不是音视频");
        }
        for n in ["a.png", "a.jpg", "a.mp4", "a.mp3", "a.txt", "a.md", "a.json", "a.rs"] {
            let a = att(n, "application/octet-stream");
            assert!(!a.is_document(), "{n} 不应判为文档");
        }
        // 老兜底逻辑仍然工作：未知二进制仍按 image 走转接
        assert!(att("a.bin", "application/octet-stream").is_image());
        // vnd.* mime 直接判文档
        assert!(att("noext", "application/vnd.openxmlformats-officedocument.wordprocessingml.document").is_document());
    }

    #[test]
    fn document_skill_and_kind_mapping() {
        assert_eq!(att("a.docx", "").document_skill(), "docx");
        assert_eq!(att("a.xlsx", "").document_skill(), "xlsx");
        assert_eq!(att("a.csv", "").document_skill(), "xlsx");
        assert_eq!(att("a.pptx", "").document_skill(), "pptx");
        assert_eq!(att("a.pdf", "").document_skill(), "pdf");
        // 无对应技能的文档给空串（notice 走 read_file 引导）
        assert_eq!(att("a.rtf", "").document_skill(), "");
        assert_eq!(att("a.rtf", "").document_kind_cn(), "Word 文档");
        assert_eq!(att("a.csv", "").document_kind_cn(), "Excel 表格");
        assert_eq!(att("a.pptx", "").document_kind_cn(), "PowerPoint 演示");
        assert_eq!(att("a.pdf", "").document_kind_cn(), "PDF 文档");
    }

    #[test]
    fn document_notice_contains_route_path_and_delivery_policy() {
        let a = att("季度报告.docx", "");
        let notice = a.document_notice("季度报告.docx");
        assert!(notice.contains(".attachments/季度报告.docx"), "notice 要给出工作区路径");
        assert!(notice.contains("read_skill(\"docx\")"), "notice 要指到对应技能");
        assert!(notice.contains("季度报告_updated.docx"), "默认副本交付策略");
        assert!(notice.contains("原地改/覆盖原文件"), "原地覆盖需用户明说");
        assert!(notice.contains("/api/workspace/raw?path=.attachments/"), "预览链接");
        // rtf 无技能：引导直接 read_file
        let r = att("说明.rtf", "");
        assert!(r.document_notice("说明.rtf").contains("read_file"));
    }

    #[test]
    fn legacy_formats_map_to_ooxml_targets() {
        assert_eq!(att("a.doc", "").legacy_target(), Some("docx"));
        assert_eq!(att("a.odt", "").legacy_target(), Some("docx"));
        assert_eq!(att("A.DOC", "").legacy_target(), Some("docx"));
        assert_eq!(att("a.xls", "").legacy_target(), Some("xlsx"));
        assert_eq!(att("a.ods", "").legacy_target(), Some("xlsx"));
        assert_eq!(att("a.ppt", "").legacy_target(), Some("pptx"));
        assert_eq!(att("a.odp", "").legacy_target(), Some("pptx"));
        // 新格式与文本格式不触发转换
        for n in ["a.docx", "a.xlsx", "a.pptx", "a.pdf", "a.csv", "a.rtf"] {
            assert_eq!(att(n, "").legacy_target(), None, "{n} 不该触发老格式转换");
        }
    }

    /// 没转换器时按真实能力说话（三档）：二进制办不了、老表格能读不能改、ODF 可预览。
    #[tokio::test]
    async fn legacy_without_converter_says_so_honestly() {
        let doc = att("合同.doc", "");
        let notice = doc
            .document_intake_notice_with(Path::new("."), "合同.doc", None)
            .await;
        assert!(notice.contains("老格式（二进制）"), "{notice}");
        assert!(notice.contains("另存为 .docx"), "{notice}");
        assert!(!notice.contains("read_skill(\"docx\")"), "无转换器时不该指技能路: {notice}");

        // .xls 引擎能读（SheetJS 实测通过）、改不了
        let xls = att("库存.xls", "");
        let notice = xls
            .document_intake_notice_with(Path::new("."), "库存.xls", None)
            .await;
        assert!(notice.contains("能读不能改"), "{notice}");
        assert!(notice.contains("read_skill(\"xlsx\")"), "{notice}");
        assert!(notice.contains("另存为 .xlsx"), "{notice}");

        // ODF：预览面板能看（doctext 支持），加工不行
        let odt = att("说明.odt", "");
        let notice = odt
            .document_intake_notice_with(Path::new("."), "说明.odt", None)
            .await;
        assert!(notice.contains("/api/workspace/raw?path=.attachments/说明.odt"), "{notice}");
        assert!(notice.contains("另存为 .docx"), "{notice}");

        // 有转换器但跑不起来（坏路径）：如实带失败原因，不假装成功
        let notice = doc
            .document_intake_notice_with(Path::new("."), "合同.doc", Some(Path::new("C:/no/such/soffice.exe")))
            .await;
        assert!(notice.contains("转换失败"), "{notice}");
    }

    /// 普通格式不该被老格式逻辑碰：照旧走技能路由。
    #[tokio::test]
    async fn normal_documents_skip_conversion() {
        let d = att("报告.docx", "");
        let notice = d
            .document_intake_notice_with(Path::new("."), "报告.docx", None)
            .await;
        assert!(notice.contains("read_skill(\"docx\")"), "{notice}");
        assert!(!notice.contains("老格式"), "{notice}");
    }

    /// 转换成功：通知指向副本、原件说明保留、交付名贴原文件词干（不是 合同_converted_updated）。
    #[test]
    fn converted_notice_points_to_copy_and_keeps_delivery_stem() {
        let doc = att("季度报告.doc", "");
        let notice = doc.converted_notice("季度报告.doc", "季度报告_converted.docx");
        assert!(notice.contains(".attachments/季度报告_converted.docx"), "{notice}");
        assert!(notice.contains("原件 .attachments/季度报告.doc 未改动"), "{notice}");
        assert!(notice.contains("默认交付 季度报告_updated.docx"), "交付名按原词干: {notice}");
        assert!(notice.contains("read_skill(\"docx\")"), "{notice}");
    }

    #[test]
    fn converted_target_never_overwrites() {
        let dir = std::env::temp_dir().join(format!("agent_conv_target_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let a = converted_target(&dir, "合同", "docx");
        assert_eq!(a.file_name().unwrap().to_str().unwrap(), "合同_converted.docx");
        std::fs::write(&a, b"x").unwrap();
        let b = converted_target(&dir, "合同", "docx");
        assert_eq!(b.file_name().unwrap().to_str().unwrap(), "合同_converted_2.docx");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
