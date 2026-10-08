;; Refused at registration: imports a WASI interface besides the snapshot.
(component
  (import "wasi:cli/environment@0.2.0"
    (instance (export "get-arguments" (func (result (list string))))))
)
