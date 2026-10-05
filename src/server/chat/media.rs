use super::*;

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

    /// 文档类附件的通知文案：合入最后一条 user 消息（随快照落盘，历史重开仍可见），
    /// 主模型据此走文档技能流水线而不是把二进制当图片内联。
    pub fn document_notice(&self, rel_name: &str) -> String {
        let skill = self.document_skill();
        let route = if skill.is_empty() {
            "这是纯文本可读的文档，直接用 read_file 读取即可".to_string()
        } else {
            format!("先 read_skill(\"{skill}\") 读取对应技能，再按技能指引的命令处理（禁止裸解 zip、禁止另造脚本、本机没有 Python）")
        };
        let stem = std::path::Path::new(&self.name)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("output");
        let deliver = match std::path::Path::new(&self.name).extension().and_then(|s| s.to_str()) {
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
}
