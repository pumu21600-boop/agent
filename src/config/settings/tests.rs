    use super::*;

    #[test]
    fn protocol_preset_inference() {
        assert_eq!(infer_protocol_preset("deepseek-v4-flash"), Some("deepseek"));
        assert_eq!(infer_protocol_preset("qwen-plus"), Some("qwen"));
        assert_eq!(infer_protocol_preset("glm-5.2"), Some("zai"));
        assert_eq!(infer_protocol_preset("gpt-5.1-codex"), Some("openai"));
        assert_eq!(infer_protocol_preset("claude-3-7-sonnet"), Some("claude"));
        assert_eq!(infer_protocol_preset("kimi-k2-thinking"), Some("moonshot"));
        assert_eq!(infer_protocol_preset("unknown-model"), None);
    }

    #[test]
    fn single_source_of_truth_and_activation() {
        let mut s = Settings::default();
        s.configs.push(Config {
            id: "cfg-1".to_string(),
            name: "商汤".to_string(),
            provider_id: "deepseek".to_string(),
            base_url: "https://token.sensenova.cn/v1".to_string(),
            model: "deepseek-v4-flash".to_string(),
            models: vec!["deepseek-v4-flash".to_string()],
            key_cipher: None,
            context_length: Some(100000),
            max_output_tokens: Some(8192),
            temperature: Some(0.7),
            top_p: Some(0.9),
            thinking: Some("on".to_string()),
            thinking_style: Some("deepseek".to_string()),
            capabilities: ModelCapabilities::default(),
            capabilities_user_set: false,
        });
        s.configs.push(Config {
            id: "cfg-2".to_string(),
            name: "千问".to_string(),
            provider_id: "qwen".to_string(),
            base_url: String::new(),
            model: "qwen-plus".to_string(),
            models: vec!["qwen-plus".to_string()],
            key_cipher: None,
            context_length: None,
            max_output_tokens: None,
            temperature: None,
            top_p: None,
            thinking: Some("off".to_string()),
            thinking_style: Some("qwen".to_string()),
            capabilities: ModelCapabilities::default(),
            capabilities_user_set: false,
        });
        s.active_config_id = Some("cfg-1".to_string());

        // 测试单一真理源：修改 active_mut 即可生效，无需手写 mirror 同步
        if let Some(c) = s.active_mut() {
            c.temperature = Some(0.2);
            c.thinking = Some("off".to_string());
        }
        assert_eq!(s.active().unwrap().temperature, Some(0.2));
        assert_eq!(s.active().unwrap().thinking.as_deref(), Some("off"));

        // 测试切换激活配置
        assert!(s.activate("cfg-2"));
        assert_eq!(s.active().unwrap().name, "千问");
        assert_eq!(s.active().unwrap().thinking_style.as_deref(), Some("qwen"));
        assert_eq!(s.effective_model(), "qwen-plus");
    }

    #[test]
    fn backward_compat_legacy_json_deserialization() {
        // 模拟 v2.4 现存真实 settings.json 格式（带顶层镜像字段和旧 keys 映射）
        let legacy_json = r#"{
            "provider_id": "deepseek",
            "base_url": "https://token.sensenova.cn/v1",
            "model": "deepseek-v4-flash",
            "models": ["deepseek-v4-flash", "glm-5.2"],
            "keys": {
                "https://token.sensenova.cn/v1": "dummy_cipher"
            },
            "configs": [
                {
                    "id": "cfg-sensenova",
                    "name": "商汤",
                    "provider_id": "deepseek",
                    "base_url": "https://token.sensenova.cn/v1",
                    "model": "deepseek-v4-flash",
                    "models": ["deepseek-v4-flash", "glm-5.2"]
                }
            ],
            "active_config_id": "cfg-sensenova",
            "workspace": "D:\\test\\ws"
        }"#;

        let mut s = parse_settings(legacy_json).expect("旧格式反序列化必须成功");
        assert_eq!(s.configs.len(), 1);
        assert_eq!(s.active().unwrap().name, "商汤");
        // 验证 URL key 迁移至 config
        s.migrate_url_keys_to_configs();
        assert_eq!(s.configs[0].key_cipher.as_deref(), Some("dummy_cipher"));
        assert_eq!(s.workspace.as_deref(), Some("D:\\test\\ws"));

        // 验证持久化输出已自动剔除 keys 和顶层镜像
        let serialized = serde_json::to_string(&s).unwrap();
        assert!(!serialized.contains("\"keys\""));
        let val: serde_json::Value = serde_json::from_str(&serialized).unwrap();
        assert!(val.get("provider_id").is_none(), "顶层镜像 provider_id 不再序列化");
        assert!(val.get("base_url").is_none(), "顶层镜像 base_url 不再序列化");
    }

    #[test]
    fn backward_compat_plugins_rename_migration() {
        // P28 目录合并迁移：旧字段 disabled_extensions → disabled_plugins（serde alias），
        // 旧信任键 ui-ext:<name> → plugins:<name>（load 迁移）；落盘只写新名。
        let legacy = r#"{
            "disabled_extensions": ["usage"],
            "trusted_plugins": { "ui-ext:board": "digest-a", "plugins:calc": "digest-b" }
        }"#;
        let mut s: Settings = parse_settings(legacy).expect("旧字段名 alias 必须可反序列化");
        assert_eq!(s.disabled_plugins, vec!["usage".to_string()], "alias 读入旧字段");
        migrate_legacy_trust_keys(&mut s.trusted_plugins);
        assert!(!s.trusted_plugins.contains_key("ui-ext:board"), "旧键必须删除");
        assert_eq!(s.trusted_plugins.get("plugins:board").map(String::as_str), Some("digest-a"), "旧键迁移到新键");
        assert_eq!(s.trusted_plugins.get("plugins:calc").map(String::as_str), Some("digest-b"), "已有新键不被迁移覆盖");
        let text = serde_json::to_string(&s).unwrap();
        assert!(text.contains("\"disabled_plugins\""), "落盘写新字段名");
        assert!(!text.contains("disabled_extensions"), "旧字段名不再写出");
        assert!(!text.contains("ui-ext:"), "旧信任键不再写出");
    }

    #[test]
    fn backward_compat_field_name_migration() {
        // P1 配置面收敛：serde alias 垫片退役，旧字段名兼容收拢到 parse_settings 单一入口。
        // ① 三组旧名各自迁入；② 新旧并存以新名为准（旧键删除）；③ 落盘不再写旧名。
        let legacy = r#"{
            "active_profile_id": "cfg-legacy",
            "profiles": [],
            "disabled_extensions": ["usage"]
        }"#;
        let s = parse_settings(legacy).expect("旧字段名必须经迁移入口读入");
        assert_eq!(s.active_config_id.as_deref(), Some("cfg-legacy"));
        assert!(s.configs.is_empty());
        assert_eq!(s.disabled_plugins, vec!["usage".to_string()]);

        let both = r#"{
            "active_config_id": "cfg-new",
            "active_profile_id": "cfg-old",
            "configs": [],
            "profiles": [],
            "disabled_plugins": ["calc"],
            "disabled_extensions": ["usage"]
        }"#;
        let s2 = parse_settings(both).expect("新旧并存必须可解析");
        assert_eq!(s2.active_config_id.as_deref(), Some("cfg-new"), "并存以新名为准");
        assert_eq!(s2.disabled_plugins, vec!["calc".to_string()]);

        let text = serde_json::to_string(&s2).unwrap();
        assert!(!text.contains("active_profile_id"), "旧字段名不再写出");
        assert!(!text.contains("\"profiles\""), "旧字段名不再写出");
        assert!(!text.contains("disabled_extensions"), "旧字段名不再写出");
    }

    #[test]
    fn features_default_all_on_and_partial_roundtrip() {
        // 插件页「内置」功能开关：旧 settings.json 无 features 字段 = 全开（行为不变）；
        // 只关一个不影响其他；序列化回读一致
        let s: Settings = serde_json::from_str("{}").unwrap();
        assert!(s.features.roles && s.features.css_tool && s.features.galaxy, "缺省全开");

        let s: Settings = serde_json::from_str(r#"{"features":{"roles":false}}"#).unwrap();
        assert!(!s.features.roles, "显式关闭生效");
        assert!(s.features.css_tool && s.features.galaxy, "未提及的开关保持默认开");

        let back = serde_json::to_string(&s.features).unwrap();
        assert!(back.contains("\"roles\":false") && back.contains("\"css_tool\":true"), "{back}");
    }

    #[test]
    fn cancel_on_disconnect_defaults_off() {
        // 审计 07：断线取消默认关——任务后台跑完，刷新可续聊；用户显式开才取消
        let s: crate::config::settings::Settings = serde_json::from_str("{}").unwrap();
        assert!(!s.features.cancel_on_disconnect);
        let on: crate::config::settings::Settings =
            serde_json::from_str(r#"{"features":{"cancel_on_disconnect":true}}"#).unwrap();
        assert!(on.features.cancel_on_disconnect);
    }