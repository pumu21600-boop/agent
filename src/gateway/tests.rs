    //! 客户端热重建机制验证：死代理 → 失败 → 环境恢复 + rebuild → 同一实例恢复连通。
    //! （bin crate 的 tests/ 目录拿不到内部符号，机制测试放这里）
    use super::*;

    fn init_logger() {
        static ONCE: std::sync::OnceLock<()> = std::sync::OnceLock::new();
        ONCE.get_or_init(|| {
            let dir = std::env::temp_dir().join("agent-test-logs");
            let _ = crate::observability::init(&dir, "gw.jsonl", format!("gw-{}", uuid::Uuid::new_v4()));
        });
    }

    #[test]
    fn build_body_vendor_styles() {
        // REGISTRY 是进程级 OnceLock：测试进程里手动加载一次（幂等）
        let _ = crate::gateway::providers::init(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("providers.json"));
        // P23 预设收敛后旧预设 id 已下架：契约知识保留在端点探测，手工构造探测线索验证
        fn legacy(id: &str) -> ChatParams {
            ChatParams { provider_id: Some(id.into()), ..Default::default() }
        }
        let msgs = json!([{"role":"user","content":"hi"}]);
        // 智谱（zai 格式，端点探测出）：on/off 对象形态；档位无意义，非 off 一律 enabled
        let mut p = ChatParams::from_preset(crate::gateway::providers::find("zhipu").unwrap());
        p.thinking = Some("on".into());
        p.max_tokens = Some(2048);
        let b = build_body("glm-4.6", &msgs, true, &p, None);
        assert_eq!(b["thinking"], json!({"type":"enabled"}));
        assert_eq!(b["max_tokens"], json!(2048));
        assert_eq!(b["stream_options"]["include_usage"], json!(true));
        p.thinking = Some("off".into());
        let b = build_body("glm-4.6", &msgs, true, &p, None);
        assert_eq!(b["thinking"], json!({"type":"disabled"}));
        p.thinking = Some("high".into());
        let b = build_body("glm-4.6", &msgs, true, &p, None);
        assert_eq!(b["thinking"], json!({"type":"enabled"}), "档位对对象格式无意义");
        // 豆包（旧预设 id，契约知识保留在端点探测）：provider_id 直判 Zai
        let mut p = legacy("doubao");
        p.thinking = Some("on".into());
        let b = build_body("doubao-seed-1.6", &msgs, true, &p, None);
        assert_eq!(b["thinking"], json!({"type":"enabled"}));
        // 混元（旧预设 id）：provider_id 直判 qwen 形态，顶层布尔
        let mut p = legacy("hunyuan");
        p.thinking = Some("off".into());
        let b = build_body("hunyuan-turbos-latest", &msgs, false, &p, None);
        assert_eq!(b["enable_thinking"], json!(false));
        assert!(b.get("thinking").is_none(), "qwen 格式不发 thinking 对象");
        p.thinking = Some("on".into());
        let b = build_body("hunyuan-turbos-latest", &msgs, false, &p, None);
        assert_eq!(b["enable_thinking"], json!(true));
        // DeepSeek（deepseek 格式，端点探测）：thinking 对象；官方 reasoner 不收 effort
        let mut p = ChatParams::from_preset(crate::gateway::providers::find("deepseek").unwrap());
        p.thinking = Some("on".into());
        let b = build_body("deepseek-reasoner", &msgs, true, &p, None);
        assert_eq!(b["thinking"], json!({"type":"enabled"}));
        assert!(b.get("reasoning_effort").is_none(), "deepseek 格式 supports_reasoning_effort=false");
        // OpenAI（openai 格式）：on→medium；low/medium/high 原样；off 不发（o 系无可靠关参数）
        let mut p = legacy("openai");
        p.thinking = Some("on".into());
        let b = build_body("gpt-5", &msgs, true, &p, None);
        assert_eq!(b["reasoning_effort"], json!("medium"));
        for lvl in ["low", "medium", "high"] {
            p.thinking = Some(lvl.into());
            let b = build_body("gpt-5", &msgs, true, &p, None);
            assert_eq!(b["reasoning_effort"], json!(lvl), "档位 {lvl} 应解析后发出");
        }
        p.thinking = Some("xhigh".into());
        let b = build_body("gpt-5", &msgs, true, &p, None);
        assert_eq!(b["reasoning_effort"], json!("high"), "xhigh 在 gpt-5 上降级为 high");
        p.thinking = Some("off".into());
        let b = build_body("gpt-5", &msgs, true, &p, None);
        assert!(b.get("reasoning_effort").is_none(), "off 无参数可发时不发");
        // GPT-5 新系 max_tokens 字段切换
        let p = ChatParams { max_tokens: Some(1024), ..legacy("openai") };
        let b = build_body("gpt-5", &msgs, true, &p, None);
        assert!(b.get("max_tokens").is_none() && b["max_completion_tokens"] == json!(1024));
        let b = build_body("gpt-4o-mini", &msgs, true, &p, None);
        assert_eq!(b["max_tokens"], json!(1024));
        // 本地（qwen-chat-template）：chat_template_kwargs.enable_thinking
        let mut p = legacy("ollama");
        p.thinking = Some("on".into());
        let b = build_body("qwen3:8b", &msgs, true, &p, None);
        assert_eq!(b["chat_template_kwargs"]["enable_thinking"], json!(true));
        p.thinking = Some("off".into());
        let b = build_body("qwen3:8b", &msgs, true, &p, None);
        assert_eq!(b["chat_template_kwargs"]["enable_thinking"], json!(false));
        // OpenAI 兼容（自定义端点预设；端点认不出 → openai 格式）
        let mut p = ChatParams::from_preset(crate::gateway::providers::find("openai_compat").unwrap());
        p.thinking = Some("high".into());
        let b = build_body("mystery-model", &msgs, true, &p, None);
        assert_eq!(b["reasoning_effort"], json!("high"));
        // Anthropic 协议（P23）：build_body 入口直接产出 Messages 形状（system 顶层/max_tokens 必填/无 stream_options）
        let mut p = legacy("anthropic");
        p.thinking = Some("on".into());
        p.max_tokens = Some(8192);
        let b = build_body("claude-sonnet-4-5", &msgs, true, &p, None);
        assert_eq!(b["max_tokens"], json!(8192));
        assert!(b.get("stream_options").is_none(), "Anthropic 不带 OpenAI stream_options");
        assert_eq!(b["thinking"]["type"], json!("enabled"), "思考开 → thinking 对象（预算见 anthropic 模块测试）");
        // 默认不发任何思考参数（无论 None、空串、全空格，全网关均不产生任何思考字段）
        for empty in [None, Some("".to_string()), Some("   ".to_string())] {
            let mut p = ChatParams::from_preset(crate::gateway::providers::find("deepseek").unwrap());
            p.thinking = empty.clone();
            let b = build_body("deepseek-v4-flash", &msgs, true, &p, None);
            assert!(b.get("thinking").is_none() && b.get("reasoning_effort").is_none(), "DeepSeek 默认不注入参数");

            let mut p = legacy("qwen");
            p.thinking = empty.clone();
            let b = build_body("qwen3.8-flash", &msgs, true, &p, None);
            assert!(b.get("enable_thinking").is_none() && b.get("thinking").is_none(), "Qwen 默认不注入参数");

            let mut p = ChatParams::from_preset(crate::gateway::providers::find("zhipu").unwrap());
            p.thinking = empty.clone();
            let b = build_body("glm-5.2", &msgs, true, &p, None);
            assert!(b.get("thinking").is_none(), "Zai 默认不注入参数");
        }
        // 采样参数：有值才发
        let p = ChatParams { temperature: Some(0.3), top_p: Some(0.9), ..Default::default() };
        let b = build_body("m", &msgs, true, &p, None);
        assert_eq!(b["temperature"], json!(0.3));
        assert_eq!(b["top_p"], json!(0.9));
        let b = build_body("m", &msgs, true, &ChatParams::default(), None);
        assert!(b.get("temperature").is_none() && b.get("top_p").is_none());
        // 非流式不带 stream_options
        let b = build_body("m", &msgs, false, &ChatParams::default(), None);
        assert!(b.get("stream_options").is_none());
    }

    // —— P21 用量台账：渠道标识与 usage 解析 ——

    #[test]
    fn host_of_strips_scheme_and_path_keeps_port() {
        assert_eq!(host_of("https://api.deepseek.com"), "api.deepseek.com");
        assert_eq!(host_of("https://api.deepseek.com/v1"), "api.deepseek.com");
        assert_eq!(host_of("http://127.0.0.1:9399/v1"), "127.0.0.1:9399");
        assert_eq!(host_of(" relay.example.com:8080 "), "relay.example.com:8080");
    }

    #[test]
    fn usage_triple_covers_openai_and_deepseek_styles() {
        // OpenAI 风格：prompt_tokens_details.cached_tokens
        let (p, c, k) = usage_triple(&json!({"usage": {"prompt_tokens": 100, "completion_tokens": 50, "prompt_tokens_details": {"cached_tokens": 64}}}));
        assert_eq!((p, c, k), (100, 50, 64));
        // DeepSeek 风格：prompt_cache_hit_tokens
        let (p, c, k) = usage_triple(&json!({"usage": {"prompt_tokens": 10, "completion_tokens": 5, "prompt_cache_hit_tokens": 8}}));
        assert_eq!((p, c, k), (10, 5, 8));
        // 渠道不回 usage：全 0（请求与延迟照记，见台账记录）
        assert_eq!(usage_triple(&json!({"choices": []})), (0, 0, 0));
        // output_tokens 兼容（个别反代用这个字段名）
        let (p, c, k) = usage_triple(&json!({"usage": {"prompt_tokens": 1, "output_tokens": 2}}));
        assert_eq!((p, c, k), (1, 2, 0));
    }

    #[test]
    fn extract_delta_reasoning_fields() {
        // DeepSeek 式 reasoning_content
        let d = extract_delta(&json!({"choices":[{"delta":{"reasoning_content":"想"}}]}));
        assert_eq!(d.reasoning.as_deref(), Some("想"));
        // 兼容字段 reasoning / reasoning_text
        let d = extract_delta(&json!({"choices":[{"delta":{"reasoning":"R"}}]}));
        assert_eq!(d.reasoning.as_deref(), Some("R"));
        let d = extract_delta(&json!({"choices":[{"delta":{"reasoning_text":"T"}}]}));
        assert_eq!(d.reasoning.as_deref(), Some("T"));
        // OpenRouter reasoning_details
        let d = extract_delta(&json!({"choices":[{"delta":{"reasoning_details":[{"type":"reasoning.text","text":"OR"}]}}]}));
        assert_eq!(d.reasoning.as_deref(), Some("OR"));
        // 空串不触发
        let d = extract_delta(&json!({"choices":[{"delta":{"reasoning_content":""}}]}));
        assert_eq!(d.reasoning, None);
    }

    #[tokio::test]
    async fn throttle_enforces_interval() {
        init_logger();
        let t = Throttle::new(60); // 60 rpm → 最小间隔 1s
        let start = Instant::now();
        t.gate().await; // 第一发直通
        t.gate().await; // 第二发必须等满 1s
        let elapsed = start.elapsed();
        assert!(elapsed >= std::time::Duration::from_millis(900), "应被节流 ≥0.9s，实际 {elapsed:?}");
        assert!(elapsed < std::time::Duration::from_millis(2500), "不应过度等待，实际 {elapsed:?}");
    }

    #[test]
    fn think_split_variants() {
        // 无标签：原样走正文
        let mut ts = TagSplit::default();
        assert_eq!(ts.feed("普通回答"), ("".into(), "普通回答".into()));
        // 标签整段：思考与正文分离
        let mut ts = TagSplit::default();
        assert_eq!(ts.feed("<think>先想想</think>答案"), ("先想想".into(), "答案".into()));
        // 跨分片断裂："<thi|nk>AB</thi|nk>CD"
        let mut ts = TagSplit::default();
        let (t1, c1) = ts.feed("<thi");
        assert_eq!((t1.as_str(), c1.as_str()), ("", ""));
        let (t2, c2) = ts.feed("nk>AB</thi");
        assert_eq!((t2.as_str(), c2.as_str()), ("AB", ""));
        let (t3, c3) = ts.feed("nk>CD");
        assert_eq!((t3.as_str(), c3.as_str()), ("", "CD"));
        let (t4, c4) = ts.finish();
        assert_eq!((t4.as_str(), c4.as_str()), ("", ""));
        // 未闭合 <think>（流在思考中被截断）：全部走思考
        let mut ts = TagSplit::default();
        let (t, c) = ts.feed("<think>还没想完");
        assert_eq!((t.as_str(), c.as_str()), ("还没想完", ""));
        // 正文里出现 "<thinking" 这类伪前缀：不误伤（'a' 即时归正文，伪标签流尾归位）
        let mut ts = TagSplit::default();
        let (t, c) = ts.feed("a<thinking");
        assert_eq!((t.as_str(), c.as_str()), ("", "a"));
        let (t2, c2) = ts.finish();
        assert_eq!((t2.as_str(), c2.as_str()), ("", "<thinking"));
        // 孤立 </think>（不在思考态）：当普通文本
        let mut ts = TagSplit::default();
        assert_eq!(ts.feed("x</think>y"), ("".into(), "xy".into()));
        // 截断在半截标签：finish 归位正文
        let mut ts = TagSplit::default();
        let (t, c) = ts.feed("尾<thi");
        assert_eq!((t.as_str(), c.as_str()), ("", "尾"));
        let (t2, c2) = ts.finish();
        assert_eq!((t2.as_str(), c2.as_str()), ("", "<thi"));
    }

    #[tokio::test]
    async fn client_rebuild_picks_up_new_proxy_config() {
        init_logger();
        let g = Gateway::new();

        // 本地 mock 渠道（曾直打 api.deepseek.com——2026-09-27 本机系统代理开关翻转后
        // 该测试稳定失败：reqwest 的代理解析跟着注册表走，外网假设不再成立）。
        // 回 401 即「网络层已通、渠道在拒假 key」；127.* 在 ProxyOverride 里天然绕代理。
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop2 = stop.clone();
        let mock = std::thread::spawn(move || {
            use std::io::Write;
            let resp = b"HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}";
            while !stop2.load(std::sync::atomic::Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut s, _)) => {
                        // 监听器是非阻塞的，Windows 上接受的 socket 会继承该标志——
                        // 不恢复阻塞模式 write_all 会撞 WouldBlock（被 let _ 吞掉）
                        let _ = s.set_nonblocking(false);
                        // 先把请求读完再回响应：不读就关，未读的入站数据会让 Windows
                        // 直接回 RST（os error 10054），客户端只会看到「连接被重置」
                        let mut buf = [0u8; 4096];
                        let _ = std::io::Read::read(&mut s, &mut buf);
                        let _ = s.write_all(resp);
                        let _ = s.flush();
                    }
                    Err(_) => std::thread::sleep(std::time::Duration::from_millis(20)),
                }
            }
        });
        let base = format!("http://127.0.0.1:{port}/v1");

        // 阶段1：代理指向死端口 → 必须失败（is_connect 分支）
        unsafe {
            std::env::set_var("HTTPS_PROXY", "http://127.0.0.1:1");
            std::env::set_var("HTTP_PROXY", "http://127.0.0.1:1");
        }
        g.rebuild_client_for_test();
        let r1 = g.list_models(&base, "sk-dummy").await;
        assert!(r1.is_err(), "死代理下应失败，实际 {:?}", r1.map(|v| v.len()));

        // 阶段2：清掉死代理 → rebuild → 同一 Gateway 实例应拿到渠道业务响应
        // （401 = 网络层已通，渠道在拒绝假 key——这正是"网络层恢复"的证据）
        unsafe {
            std::env::remove_var("HTTPS_PROXY");
            std::env::remove_var("HTTP_PROXY");
        }
        g.rebuild_client_for_test();
        let r2 = g.list_models(&base, "sk-dummy").await;
        let msg = r2.err().map(|e| format!("{e:#}")).unwrap_or_default();
        assert!(
            msg.contains("401") && !msg.contains("请求渠道失败"),
            "重建后应拿到渠道 401（网络层恢复），实际: {msg}"
        );
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        mock.join().unwrap();
    }