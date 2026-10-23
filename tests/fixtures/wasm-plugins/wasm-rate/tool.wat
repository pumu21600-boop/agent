;; WASM 宿主函数示例 ①：http_get（汇率）
;;
;; 契约：manifest 声明 "permissions": ["http"] 才能导入 host.http_get——
;; 未声明 = 实例化失败（权限闸）。工具 rate 无参数：调用 http_get 抓固定
;; 汇率接口，宿主把响应体（≤64KB）写回插件内存并返回 packed(ptr,len)，
;; 本插件原样转发作为 run 的返回。
;;
;; 内存布局：0 = describe JSON（0 终止）；8192 = 请求 URL（0 终止）；
;; 16384 起 = bump allocator 堆（宿主写结果用）。
(module
  (import "host" "http_get" (func $http_get (param i32 i32) (result i64)))
  (memory (export "memory") 1)
  (global $heap (mut i32) (i32.const 16384))
  (data (i32.const 0) "{\"tools\":[{\"name\":\"rate\",\"description\":\"查询美元兑人民币的参考汇率。宿主函数 http_get 示例：GET 公开汇率接口并原样返回响应。\",\"schema\":{\"type\":\"object\",\"properties\":{}}}]}\00")
  (data (i32.const 8192) "https://open.er-api.com/v6/latest/USD\00")
  ;; bump allocator：宿主函数的结果缓冲从这里分配
  (func (export "alloc") (param $n i32) (result i32)
    (local $p i32)
    (local.set $p (global.get $heap))
    (global.set $heap (i32.add (global.get $heap) (local.get $n)))
    (local.get $p))
  (func $pack (param $p i32) (param $l i32) (result i64)
    (i64.or (i64.shl (i64.extend_i32_u (local.get $p)) (i64.const 32))
            (i64.extend_i32_u (local.get $l))))
  ;; len=0：宿主按 0 终止字符串读 describe
  (func (export "describe") (result i64)
    (call $pack (i32.const 0) (i32.const 0)))
  (func (export "run") (param $a1 i32) (param $a2 i32) (param $p1 i32) (param $p2 i32) (result i64)
    ;; URL 长 37 字节；http_get 的返回值就是工具结果，直接转发
    (call $http_get (i32.const 8192) (i32.const 37))))
