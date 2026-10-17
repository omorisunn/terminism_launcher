;; Minimal terminism wasm guest (WAT source, loaded directly in dev).
;; Exports: memory, alloc(len)->ptr, init(), on_event(name_ptr,name_len,payload_ptr,payload_len)->i32
;; Imports: env.log(ptr,len)
(module
  (import "env" "log" (func $log (param i32 i32)))
  (memory (export "memory") 1)
  (global $top (mut i32) (i32.const 1024))
  (data (i32.const 0) "wasm-hello init")
  (data (i32.const 32) "wasm-hello event")

  ;; bump allocator over the guest linear memory
  (func (export "alloc") (param $len i32) (result i32)
    (local $p i32)
    (local.set $p (global.get $top))
    (global.set $top (i32.add (global.get $top) (local.get $len)))
    (local.get $p))

  (func (export "dealloc") (param $ptr i32) (param $len i32)
    (global.set $top (local.get $ptr)))

  (func (export "init")
    (call $log (i32.const 0) (i32.const 15)))

  (func (export "on_event") (param i32 i32 i32 i32) (result i32)
    (call $log (i32.const 32) (i32.const 16))
    (i32.const 0))
)
