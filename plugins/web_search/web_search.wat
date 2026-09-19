(module
  ;; P19 契约试点：web_search 以目录形态工具插件落 plugins/（manifest + 恰好一个 .wat）。
  ;; WASM 引擎 v0 零宿主函数 = 无网络，本工具不做真搜索：验证清单/加载/审批/调用链路，
  ;; 被调用时返回说明文本引导模型改用内置 web_search。
  ;; ABI 约定：len=0 表示 0 终止字符串（宿主扫描）
  (memory (export "memory") 1)
  (data (i32.const 2048) "{\"tools\":[{\"name\":\"search\",\"description\":\"契约试点占位（P19）：插件壳里的 web_search，无网络能力不做真搜索——请改用内置 web_search 工具。本插件用于验证目录形态工具插件的清单/加载/审批/调用链路。\",\"schema\":{\"type\":\"object\",\"properties\":{\"query\":{\"type\":\"string\",\"description\":\"搜索关键词\"}}}}]}\00")
  (data (i32.const 6144) "{\"text\":\"我是 web_search 的插件壳试点（P19）：WASM 纯计算沙箱没有网络能力，做不了真搜索。请改用内置 web_search 工具完成联网搜索；本插件的存在是为了验证插件契约的目录形态工具链路。\"}\00")
  (func (export "alloc") (param $n i32) (result i32) (i32.const 8192))
  (func $pack (param $p i32) (param $l i32) (result i64)
    (i64.or (i64.shl (i64.extend_i32_u (local.get $p)) (i64.const 32))
            (i64.extend_i32_u (local.get $l))))
  (func (export "describe") (result i64)
    (call $pack (i32.const 2048) (i32.const 0)))
  (func (export "run") (param $a1 i32) (param $a2 i32) (param $p1 i32) (param $p2 i32) (result i64)
    (call $pack (i32.const 6144) (i32.const 0)))
)
