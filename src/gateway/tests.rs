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
    fn thinking_inflates_max_tokens_budget() {
        // 思考开启时上游把 reasoning+正文算进同一个 max_tokens，用户配的
        // 「最大输出长度」会被思考吃掉——发送值 = 配置值 + max(配置值, 8192)。
        // 思考关闭/未配 → 原值（此时上限本就只约束正文）。
        let mut p = ChatParams { max_tokens: Some(4096), thinking: Some("high".into()), ..Default::default() };
        assert_eq!(effective_max_tokens(&p), Some(12288));
        p.thinking = Some("on".into());
        assert_eq!(effective_max_tokens(&p), Some(12288));
        p.thinking = Some("off".into());
        assert_eq!(effective_max_tokens(&p), Some(4096), "思考关闭不扩容");
        p.thinking = None;
        assert_eq!(effective_max_tokens(&p), Some(4096), "未配思考档位不扩容");
        p.max_tokens = Some(12600);
        p.thinking = Some("high".into());
        assert_eq!(effective_max_tokens(&p), Some(25200), "配置值大于 8192 时按配置值翻倍");
        p.max_tokens = None;
        assert_eq!(effective_max_tokens(&p), None);
    }

    #[test]
    fn build_body_vendor_styles() {
        // REGISTRY 是进程级 OnceLock：测试进程里手动加载一次（幂等）
        let _ = crate::gateway::providers::init(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("providers.json"));
        // 预设收敛后旧预设 id 已下架：契约知识保留在端点探测，手工构造探测线索验证
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
        // Anthropic 协议：build_body 入口直接产出 Messages 形状（system 顶层/max_tokens 必填/无 stream_options）
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

    // —— 用量台账：渠道标识与 usage 解析 ——

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
        // 2026-10-08 修：Anthropic 的 input_tokens 不含 cache_read/cache_creation，
        // 而 OpenAI 的 prompt_tokens 是含缓存的总输入——补齐，否则锚点系统性低估
        let (p, c, k) = usage_triple(&json!({"usage": {
            "input_tokens": 100, "output_tokens": 50,
            "cache_read_input_tokens": 64, "cache_creation_input_tokens": 32,
        }}));
        assert_eq!((p, c, k), (196, 50, 64), "100+64+32；cached 只报 cache_read");
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
    async fn loopback_base_url_bypasses_proxy() {
        // 2026-10-08 修：环回渠道（本地模型服务 ollama/lmstudio/mnn/llamacpp 全是
        // 127.0.0.1 端点）原先会走系统/环境代理——代理连自己的 localhost 回 502，
        // 用户看到「渠道返回 502」而渠道本身没毛病。reqwest 默认不排除环回（注释里
        // 写了多年的「ProxyOverride 环回绕代理」从未实现），现在 Gateway 对环回
        // base_url 专用 no_proxy 客户端。本测试钉住：环境里挂着（死）代理时，
        // 环回请求照旧直达渠道。
        init_logger();
        let g = Gateway::new();

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop2 = stop.clone();
        let mock = std::thread::spawn(move || {
            use std::io::Write;
            let resp = b"HTTP/1.1 401 Unauthorized
Content-Type: application/json
Content-Length: 2

{}";
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

        // 环境里挂一个死代理——环回请求必须绕过它直达 mock（401 = 网络层已通）
        unsafe {
            std::env::set_var("HTTPS_PROXY", "http://127.0.0.1:1");
            std::env::set_var("HTTP_PROXY", "http://127.0.0.1:1");
        }
        g.rebuild_client_for_test();
        let r1 = g.list_models(&base, "sk-dummy").await;
        let msg1 = r1.err().map(|e| format!("{e:#}")).unwrap_or_default();
        assert!(
            msg1.contains("401"),
            "环回 base_url 应绕过（死）代理直达渠道拿到 401，实际: {msg1}"
        );
        // 清掉代理再 rebuild：行为不变（环回客户端与代理配置无关）
        unsafe {
            std::env::remove_var("HTTPS_PROXY");
            std::env::remove_var("HTTP_PROXY");
        }
        g.rebuild_client_for_test();
        let r2 = g.list_models(&base, "sk-dummy").await;
        let msg2 = r2.err().map(|e| format!("{e:#}")).unwrap_or_default();
        assert!(msg2.contains("401"), "清掉代理后仍应拿到 401，实际: {msg2}");
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        mock.join().unwrap();
    }

    /// 环回主机判定：本地模型端点的各种写法都要认出来。
    #[test]
    fn loopback_base_url_detection() {
        for u in [
            "http://127.0.0.1:11434/v1",
            "http://localhost:1234/v1",
            "http://127.5.6.7:8080",
            "http://user:pass@127.0.0.1:8080/v1",
            "http://[::1]:8080/v1",
            "127.0.0.1:8080",
        ] {
            assert!(is_loopback_base_url(u), "{u} 应判为环回");
        }
        for u in [
            "https://api.deepseek.com/v1",
            "https://127.0.0.1.evil.com/v1",
            "https://open.bigmodel.cn/api/paas/v4",
            "https://2127.0.0.1/v1",
        ] {
            assert!(!is_loopback_base_url(u), "{u} 不应判为环回");
        }
    }
    #[test]
    fn take_line_survives_utf8_split_across_chunks() {
        // N1：网络分片边界不保证 UTF-8 字符完整。「你」= E4 BD A0、「好」= E5 A5 BD，
        // 从「你」第三个字节处切开——旧写法（逐分片 from_utf8_lossy）会把两个半截
        // 汉字碎成 U+FFFD 永久进正文。take_line 按字节攒完整行再解码，不该出替换符。
        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice("data: {\"content\":\"\u{4f60}".as_bytes());
        buf.truncate(buf.len() - 1); // 砍掉「你」的最后一个字节，模拟分片切在字符中间
        assert_eq!(take_line(&mut buf), None, "半个汉字 + 半行不该出行");
        buf.extend_from_slice(b"\xA0"); // 补齐「你」被砍掉的尾字节（裸字节，不是 U+00A0 字符）
        buf.extend_from_slice("好".as_bytes());
        buf.extend_from_slice("\"}\n".as_bytes());
        let line = take_line(&mut buf).expect("补齐后应出完整行");
        assert_eq!(line, "data: {\"content\":\"你好\"}");
        assert!(!line.contains('\u{FFFD}'), "不该出现 U+FFFD 替换符: {line:?}");
        assert_eq!(take_line(&mut buf), None, "行已取完");
        // 回归：\r\n 行尾与空行照旧处理
        buf.extend_from_slice(b"data: x\r\n\r\n");
        assert_eq!(take_line(&mut buf).as_deref(), Some("data: x"));
        assert_eq!(take_line(&mut buf).as_deref(), Some(""));
    }

    /// 请求体摘要：必须丢掉 messages 正文（那是日志体积的真凶），但留住排查维度。
    ///
    /// 回归 2026-10-06 用户实锤「时间线刷不出来」：完整 body 单条 2MB+，每步一条
    /// 就把 20MB 轮转上限撞穿，task.start 被归档删掉 → 时间线空白。摘要后单条
    /// 应只有几百字节。
    #[test]
    fn request_summary_drops_message_text_keeps_shape() {
        let body = json!({
            "model": "step-5-preview",
            "stream": true,
            "messages": [
                {"role": "system", "content": "x".repeat(20000)},
                {"role": "user", "content": "y".repeat(30000)},
            ],
            "tools": [{"type":"function"},{"type":"function"}],
        });
        let s = crate::gateway::request_summary(&body);
        let text = s.to_string();
        assert!(text.len() < 1000, "摘要必须远小于原体：{} 字节", text.len());
        assert!(!text.contains('x') || !text.contains("xxxxxxxxxx"), "正文不得进摘要");
        assert_eq!(s["model"], "step-5-preview");
        assert_eq!(s["messages"], 2);
        assert_eq!(s["message_chars"], 50000);
        assert_eq!(s["tools"], 2);
        // 每条消息的角色与字符数要留（定位「哪一轮把历史撑大了」靠它）
        assert_eq!(s["per_message"][0]["role"], "system");
        assert_eq!(s["per_message"][0]["chars"], 20000);
        assert_eq!(s["per_message"][1]["role"], "user");
    }

    /// prompt_cache_key：仅官方 OpenAI 端点发，且随会话 id 走（同会话前缀命中同一
    /// 缓存条目）；中转/国产端点一个都不发（普遍 400）；none 档连键也不发。
    #[test]
    fn prompt_cache_key_gating() {
        let msgs = json!([{"role":"user","content":"hi"}]);
        let compat = compat::resolve(Some("openai"), Some("https://api.openai.com/v1"), "gpt-5", None);
        let p = ChatParams::default();
        let b = build_body_with_compat("gpt-5", &msgs, true, &p, None, &compat, "sess_abc");
        assert_eq!(b["prompt_cache_key"], json!("sess_abc"), "官方端点 + short 档：带会话级缓存键");
        // 会话 id 缺省（辅助调用没归属）→ 不发键，不留 undefined 字段
        let b = build_body_with_compat("gpt-5", &msgs, true, &p, None, &compat, "  ");
        assert!(b.get("prompt_cache_key").is_none());
        // 中转端点：即便 retention=long 也不发（无 supports 标志）
        let relay = compat::resolve(Some("custom"), Some("https://relay.example.com/v1"), "gpt-5", None);
        let pl = ChatParams { cache_retention: CacheRetention::Long, ..Default::default() };
        let b = build_body_with_compat("gpt-5", &msgs, true, &pl, None, &relay, "sess_abc");
        assert!(b.get("prompt_cache_key").is_none(), "中转不发 prompt_cache_key");
        assert!(b.get("prompt_cache_retention").is_none(), "中转也不发 24h 保留");
        // none 档：官方也不发
        let pn = ChatParams { cache_retention: CacheRetention::None, ..Default::default() };
        let b = build_body_with_compat("gpt-5", &msgs, true, &pn, None, &compat, "sess_abc");
        assert!(b.get("prompt_cache_key").is_none(), "用户关缓存 → 键也不发");
        // long 档 + 官方：24h 保留 + 键
        let b = build_body_with_compat("gpt-5", &msgs, true, &pl, None, &compat, "sess_abc");
        assert_eq!(b["prompt_cache_retention"], json!("24h"));
        assert_eq!(b["prompt_cache_key"], json!("sess_abc"));
    }

    /// prompt_cache_key 截断到 64 字符（官方规范）；同会话内稳定即可，不求全局唯一。
    #[test]
    fn prompt_cache_key_clamped_to_64_chars() {
        let long = "s".repeat(100);
        assert_eq!(clamp_prompt_cache_key(&long).len(), 64);
        assert_eq!(clamp_prompt_cache_key("sess_abc"), "sess_abc");
        assert_eq!(clamp_prompt_cache_key("  sess_abc  "), "sess_abc", "trim 空白");
    }
