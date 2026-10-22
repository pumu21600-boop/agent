;; WASM 宿主函数示例 ③：storage_get / storage_set（持久记事本）
;;
;; 契约：manifest 声明 "permissions": ["storage"] 才能导入两个 storage 宿主函数。
;; 工具 save（参数 {"text":"..."}）：把文本存进本插件命名空间的 note 键，
;; 随后立刻读回并返回（演示「写→读」闭环）；工具 load：读回上次保存的文本。
;; 两次调用是两个独立 WASM 实例——数据能跨调用、跨重启存活，靠的是宿主侧
;; plugin_data 持久存储，不是插件内存。
;;
;; 分发：payload JSON 里扫 "tool":"load" 判 load 工具（演示级解析：参数值里
;; 出现同样的字节串会误判，示例可接受）。
;;
;; 内存布局：0 = describe；8192 = 探针 "tool":"load"（13 字节）；
;; 8208 = 探针 "text":"（8 字节）；8224 = 键名 note；
;; 9216 = 错误串；16384 起 = 堆。
(module
  (import "host" "storage_get" (func $storage_get (param i32 i32) (result i64)))
  (import "host" "storage_set" (func $storage_set (param i32 i32 i32 i32) (result i64)))
  (memory (export "memory") 1)
  (global $heap (mut i32) (i32.const 16384))
  (data (i32.const 0) "{\"tools\":[{\"name\":\"save\",\"description\":\"保存一段文本到插件的持久存储（重启不丢），保存后读回作为结果。宿主函数 storage_set 示例。\",\"schema\":{\"type\":\"object\",\"properties\":{\"text\":{\"type\":\"string\"}},\"required\":[\"text\"]}},{\"name\":\"load\",\"description\":\"读回上次保存的文本。宿主函数 storage_get 示例。\",\"schema\":{\"type\":\"object\",\"properties\":{}}}]}\00")
  (data (i32.const 8192) "\"tool\":\"load\"\00")
  (data (i32.const 8208) "\"text\":\"\00")
  (data (i32.const 8224) "note\00")
  (data (i32.const 9216) "{\"error\":\"missing text arg\"}\00")
  (func (export "alloc") (param $n i32) (result i32)
    (local $p i32)
    (local.set $p (global.get $heap))
    (global.set $heap (i32.add (global.get $heap) (local.get $n)))
    (local.get $p))
  (func $pack (param $p i32) (param $l i32) (result i64)
    (i64.or (i64.shl (i64.extend_i32_u (local.get $p)) (i64.const 32))
            (i64.extend_i32_u (local.get $l))))
  (func (export "describe") (result i64)
    (call $pack (i32.const 0) (i32.const 0)))
  (func $find (param $hay i32) (param $hay_len i32) (param $nd i32) (param $nd_len i32) (result i32)
    (local $i i32) (local $j i32) (local $ok i32)
    (if (i32.lt_u (local.get $hay_len) (local.get $nd_len))
      (then (return (i32.const -1))))
    (block $done
      (loop $outer
        (local.set $j (i32.const 0))
        (local.set $ok (i32.const 1))
        (block $cmp_done
          (loop $cmp
            (br_if $cmp_done (i32.ge_u (local.get $j) (local.get $nd_len)))
            (if (i32.ne (i32.load8_u (i32.add (i32.add (local.get $hay) (local.get $i)) (local.get $j)))
                        (i32.load8_u (i32.add (local.get $nd) (local.get $j))))
              (then
                (local.set $ok (i32.const 0))
                (br $cmp_done)))
            (local.set $j (i32.add (local.get $j) (i32.const 1)))
            (br $cmp)))
        (if (local.get $ok)
          (then (return (i32.add (local.get $hay) (local.get $i)))))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br_if $done (i32.gt_u (local.get $i) (i32.sub (local.get $hay_len) (local.get $nd_len))))
        (br $outer)))
    (i32.const -1))
  (func $scan_len (param $start i32) (result i32)
    (local $len i32)
    (block $done
      (loop $l
        (br_if $done (i32.eq (i32.load8_u (i32.add (local.get $start) (local.get $len))) (i32.const 34)))
        (local.set $len (i32.add (local.get $len) (i32.const 1)))
        (br_if $done (i32.ge_u (local.get $len) (i32.const 1024)))
        (br $l)))
    (local.get $len))
  (func (export "run") (param $a1 i32) (param $a2 i32) (param $p1 i32) (param $p2 i32) (result i64)
    (local $hit i32) (local $vs i32)
    ;; load 工具：读回 note 键
    (if (i32.ne (call $find (local.get $p1) (local.get $p2) (i32.const 8192) (i32.const 13)) (i32.const -1))
      (then (return (call $storage_get (i32.const 8224) (i32.const 4)))))
    ;; save 工具：扫出 text 值 → storage_set（结果丢弃）→ storage_get 读回转发
    (local.set $hit (call $find (local.get $p1) (local.get $p2) (i32.const 8208) (i32.const 8)))
    (if (i32.eq (local.get $hit) (i32.const -1))
      (then (return (call $pack (i32.const 9216) (i32.const 28)))))
    (local.set $vs (i32.add (local.get $hit) (i32.const 8)))
    (drop (call $storage_set (i32.const 8224) (i32.const 4)
                             (local.get $vs) (call $scan_len (local.get $vs))))
    (call $storage_get (i32.const 8224) (i32.const 4))))
