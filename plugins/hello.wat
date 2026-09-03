(module
  ;; 冒烟 WASM 插件（017）：导出 hello 工具，返回固定问候
  ;; ABI 约定：len=0 表示 0 终止字符串（宿主扫描）
  (memory (export "memory") 1)
  (data (i32.const 2048) "{\"tools\":[{\"name\":\"hello\",\"description\":\"greeting from wasm plugin\",\"schema\":{\"type\":\"object\",\"properties\":{\"name\":{\"type\":\"string\"}}}}]}\00")
  (data (i32.const 4096) "{\"text\":\"hello from wasm plugin!\"}\00")
  (func (export "alloc") (param $n i32) (result i32) (i32.const 8192))
  (func $pack (param $p i32) (param $l i32) (result i64)
    (i64.or (i64.shl (i64.extend_i32_u (local.get $p)) (i64.const 32))
            (i64.extend_i32_u (local.get $l))))
  (func (export "describe") (result i64)
    (call $pack (i32.const 2048) (i32.const 0)))
  (func (export "run") (param $a1 i32) (param $a2 i32) (param $p1 i32) (param $p2 i32) (result i64)
    (call $pack (i32.const 4096) (i32.const 0)))
)
