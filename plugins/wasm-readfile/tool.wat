;; WASM 宿主函数示例 ②：fs_read（读工作区文件）
;;
;; 契约：manifest 声明 "permissions": ["fs_read"] 才能导入 host.fs_read。
;; 工具 read 参数 {"path":"相对工作区的路径"}：从 payload JSON 里扫出
;; "path":"..." 的值（演示级解析：不支持转义引号），交给宿主读（判界/上限/
;; UTF-8 校验都在宿主侧），原样转发结果。
;;
;; 内存布局：0 = describe；8192 = 探针串 "path":"（8 字节）；
;; 9216 = 错误串（缺参数时返回）；16384 起 = 堆。
(module
  (import "host" "fs_read" (func $fs_read (param i32 i32) (result i64)))
  (memory (export "memory") 1)
  (global $heap (mut i32) (i32.const 16384))
  (data (i32.const 0) "{\"tools\":[{\"name\":\"read\",\"description\":\"读取当前工作区里的一个文本文件（≤256KB、UTF-8）。参数 path 为相对工作区的路径。宿主函数 fs_read 示例：越出工作区的路径会被宿主拒绝。\",\"schema\":{\"type\":\"object\",\"properties\":{\"path\":{\"type\":\"string\",\"description\":\"相对工作区的文件路径\"}},\"required\":[\"path\"]}}]}\00")
  (data (i32.const 8192) "\"path\":\"\00")
  (data (i32.const 9216) "{\"error\":\"missing path arg\"}\00")
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
  ;; 在 [hay, hay+hay_len) 找与 [nd, nd+nd_len) 逐字节相同的子串；
  ;; 命中返回命中处绝对地址，未命中返回 -1
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
  ;; 从 start 起数到第一个 '"'（JSON 字符串值结尾）的长度；1KB 上限防失控
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
    (local.set $hit (call $find (local.get $p1) (local.get $p2) (i32.const 8192) (i32.const 8)))
    (if (i32.eq (local.get $hit) (i32.const -1))
      (then (return (call $pack (i32.const 9216) (i32.const 28)))))
    (local.set $vs (i32.add (local.get $hit) (i32.const 8)))
    (call $fs_read (local.get $vs) (call $scan_len (local.get $vs)))))
