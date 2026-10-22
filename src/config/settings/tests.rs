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
        // 目录合并迁移：旧字段 disabled_extensions → disabled_plugins（serde alias）；
        // 落盘只写新名。（2026-10-08：信任表 trusted_plugins 随验签机制移除，旧键
        // trusted_plugins/ui-ext:* 由 serde 忽略未知字段自然兼容，不再迁移。）
        let legacy = r#"{
            "disabled_extensions": ["usage"],
            "trusted_plugins": { "ui-ext:board": "digest-a" }
        }"#;
        let s: Settings = parse_settings(legacy).expect("旧字段名 alias 必须可反序列化");
        assert_eq!(s.disabled_plugins, vec!["usage".to_string()], "alias 读入旧字段");
        let text = serde_json::to_string(&s).unwrap();
        assert!(text.contains("\"disabled_plugins\""), "落盘写新字段名");
        assert!(!text.contains("disabled_extensions"), "旧字段名不再写出");
        assert!(!text.contains("ui-ext:"), "旧信任键不再写出");
    }

    #[test]
    fn backward_compat_v0_top_level_rescue() {
        // 施工单 001（09-02）的最初格式：顶层五字段，无 configs 无 profiles。
        // 顶层 key 直接挂到挽救出的配置（不依赖 providers 预设兜底——那条路在
        // 预设 id 被收敛删除的场合会把 key 卡死在过渡槽里静默丢失）。
        let v0 = r#"{
            "provider_id": "deepseek",
            "base_url": "",
            "model": "deepseek-chat",
            "models": ["deepseek-chat", "deepseek-reasoner"],
            "key_cipher": "v0-dpapi-cipher"
        }"#;
        let s = settings_from_text(v0).expect("v0 顶层格式必须可挽救");
        assert_eq!(s.configs.len(), 1, "顶层字段挽救成一套配置");
        let c = s.active().expect("挽救后必须有激活配置");
        assert_eq!(c.provider_id, "deepseek");
        assert_eq!(c.model, "deepseek-chat");
        assert_eq!(c.models, vec!["deepseek-chat".to_string(), "deepseek-reasoner".to_string()]);
        assert_eq!(c.key_cipher.as_deref(), Some("v0-dpapi-cipher"), "v0 顶层 key 三跳归位到配置");
        assert!(s.keys.is_empty(), "keys 过渡槽归位后清空");
        // 落盘不写顶层镜像与过渡槽
        let val: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert!(val.get("provider_id").is_none(), "顶层镜像 provider_id 不再序列化");
        assert!(val.get("key_cipher").is_none(), "顶层 key_cipher 过渡槽不序列化");
        assert!(val.get("keys").is_none(), "keys 过渡槽不序列化");
    }

    #[test]
    fn backward_compat_pre_b84_thinking_budget_dropped() {
        // 09-03 一天窗口的中间格式（P8.2 加 Config.thinking_budget，同日 P8.4 照抄
        // OpenClaw 覆盖删除）：有意不迁——数值语义已被档位化 thinking + thinking_style
        // 形态取代，无迁移目标。解析必须不炸、thinking 档位保留、死键静默丢弃。
        let legacy = r#"{
            "configs": [{
                "id": "cfg-mid", "name": "中间格式", "provider_id": "deepseek",
                "base_url": "https://api.deepseek.com", "model": "deepseek-chat",
                "thinking": "high", "thinking_budget": 4096
            }]
        }"#;
        let s = parse_settings(legacy).expect("中间格式必须可解析");
        assert_eq!(s.configs[0].thinking.as_deref(), Some("high"), "thinking 档位保留");
        assert_eq!(s.configs[0].thinking_style, None);
        let text = serde_json::to_string(&s).unwrap();
        assert!(!text.contains("thinking_budget"), "死字段落盘不写");
    }

    #[test]
    fn backward_compat_field_name_migration() {
        // 配置面收敛：serde alias 垫片退役，旧字段名兼容收拢到 parse_settings 单一入口。
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
    #[test]
    fn feature_set_roundtrip_and_unknown_key_errors() {
        // 权限放开轮（2026-10-02）：插件开关 ↔ settings.features 联动。set 是唯一写入口，
        // 未知键必须报错——serde 对未知字段静默丢弃，「开关写了却没生效」比报错更难查
        let mut f = crate::config::settings::FeatureSettings::default();
        for key in crate::plugin::ui::FEATURE_KEYS {
            f.set(key, false).unwrap();
            let back = serde_json::to_string(&f).unwrap();
            assert!(!back.contains(&format!("\"{key}\":true")), "{key} 应被关掉: {back}");
            f.set(key, true).unwrap();
        }
        let err = f.set("no_such_feature", true).unwrap_err();
        assert!(err.contains("no_such_feature") && err.contains("roles"), "{err}");
    }

    /// 外部改动重载与启动 load 必须走**同一把尺子**：同一份 JSON，两边解析出的状态
    /// 逐字节相同。否则「重启后是一种状态、热加载后是另一种」，比不热加载更难查
    /// （2026-10-05 真机实锤：agent 直接改 settings.json，界面不刷新）。
    #[test]
    fn settings_from_text_matches_load_migration_semantics() {
        // 老版形态：无 configs 数组，全靠顶层字段 + 旧字段名（2026-10-08：旧信任键
        // trusted_plugins/ui-ext:* 已随验签移除，serde 忽略未知字段自然兼容）
        let legacy = r#"{
            "active_profile_id": "p1",
            "profiles": [{"id":"p1","name":"老配置","provider_id":"deepseek","base_url":"https://api.deepseek.com","model":"deepseek-chat"}],
            "disabled_extensions": ["usage"],
            "trusted_plugins": {"ui-ext:demo":"abc123"}
        }"#;
        let s = settings_from_text(legacy).expect("老版文本应可解析");
        assert_eq!(s.configs.len(), 1, "旧 profiles 应迁成 configs");
        assert_eq!(s.configs[0].name, "老配置");
        assert_eq!(s.active_config_id.as_deref(), Some("p1"), "旧 active_profile_id 应迁成激活项");
        assert_eq!(s.disabled_plugins, vec!["usage".to_string()], "旧 disabled_extensions 应迁名");
    }

    /// 空/坏 JSON 一律解析失败——外部监听据此「保持内存现状」，绝不拿半截文件覆盖
    #[test]
    fn settings_from_text_rejects_broken_text() {
        assert!(settings_from_text("").is_err());
        assert!(settings_from_text("   ").is_err());
        assert!(settings_from_text("{ \"configs\": [").is_err(), "截断的半截 JSON 必须拒");
    }

    /// 合法最小 JSON 也要能解析出「有激活配置」的可用状态（热加载后不能处于残废态）
    #[test]
    fn settings_from_text_minimal_text_is_usable() {
        let s = settings_from_text("{}").expect("空对象应可用默认值解析");
        assert!(!s.configs.is_empty(), "应自动补一套配置");
        assert!(s.active_config_id.is_some(), "应有激活配置");
    }
