;; A panel plugin with one command, `fixture.open`, that opens the `fixture` panel. The panel
;; has one section with a text item and a bar. The bar's ratio is attached / terminals and its
;; width is the requested width, so a test can see that the overview reached the plugin.
;; Its settings table is `fixture`, and `configure` asks for a refresh of 100 ms per byte of
;; the settings JSON it receives, so a test can see which settings arrived.
(component
  (type $ui (instance
    (type $overflow (enum "path" "text" "keep"))
    (export "overflow" (type $overflow-e (eq $overflow)))
    (type $optional-string (option string))
    (type $segment (record
      (field "text" string)
      (field "fg" $optional-string)
      (field "bg" $optional-string)
      (field "bold" bool)
      (field "italic" bool)
      (field "overflow" $overflow-e)
    ))
    (export "segment" (type (eq $segment)))
    (type $plugin-settings (record (field "key" string) (field "defaults-json" string)))
    (export "plugin-settings" (type (eq $plugin-settings)))
  ))
  (import "power:terminal-ui/types@0.1.0" (instance $ui-types (type $ui)))
  (alias export $ui-types "segment" (type $segment))
  (alias export $ui-types "plugin-settings" (type $plugin-settings))

  (type $panel-types (instance
    (alias outer 1 $segment (type $segment))
    (export "segment" (type $segment-e (eq $segment)))
    (type $optional-string (option string))
    (type $command-scope (enum "box" "tab"))
    (export "command-scope" (type $command-scope-e (eq $command-scope)))
    (type $command (record
      (field "id" string)
      (field "title" string)
      (field "description" string)
      (field "scope" $command-scope-e)
      (field "default-keys" (list string))
    ))
    (export "command" (type (eq $command)))
    (type $effect (variant (case "none") (case "open-panel" string)))
    (export "effect" (type (eq $effect)))
    (type $box-overview (record
      (field "id" string)
      (field "name" string)
      (field "terminals" u32)
      (field "attached" u32)
      (field "agents-working" u32)
      (field "runs-active" u32)
    ))
    (export "box-overview" (type $box-overview-e (eq $box-overview)))
    (type $run-status (enum "active" "completed" "stalled" "archived"))
    (export "run-status" (type $run-status-e (eq $run-status)))
    (type $run-overview (record
      (field "run" string)
      (field "box-id" string)
      (field "title" string)
      (field "completed" u32)
      (field "total" u32)
      (field "workers" u32)
      (field "eta-secs" (option u64))
      (field "status" $run-status-e)
      (field "paused" bool)
    ))
    (export "run-overview" (type $run-overview-e (eq $run-overview)))
    (type $usage-window (record
      (field "label" string)
      (field "used-percent" f32)
      (field "resets-at" (option u64))
    ))
    (export "usage-window" (type $usage-window-e (eq $usage-window)))
    (type $model-usage (record
      (field "provider" string)
      (field "model" $optional-string)
      (field "windows" (list $usage-window-e))
    ))
    (export "model-usage" (type $model-usage-e (eq $model-usage)))
    (type $system-stats (record
      (field "cpu-percent" (option f32))
      (field "mem-used" u64)
      (field "mem-total" u64)
    ))
    (export "system-stats" (type $system-stats-e (eq $system-stats)))
    (type $totals (record
      (field "agents-working" u32)
      (field "terminals" u32)
      (field "attached" u32)
    ))
    (export "totals" (type $totals-e (eq $totals)))
    (type $overview (record
      (field "boxes" (list $box-overview-e))
      (field "runs" (list $run-overview-e))
      (field "usage" (list $model-usage-e))
      (field "system" $system-stats-e)
      (field "totals" $totals-e)
      (field "at" u64)
    ))
    (export "overview" (type (eq $overview)))
    (type $bar (record
      (field "ratio" f32)
      (field "width" u32)
      (field "fg" $optional-string)
      (field "bg" $optional-string)
    ))
    (export "bar" (type $bar-e (eq $bar)))
    (type $item (variant (case "text" $segment-e) (case "bar" $bar-e) (case "spacer")))
    (export "item" (type $item-e (eq $item)))
    (type $line (record (field "left" (list $item-e)) (field "right" (list $item-e))))
    (export "line" (type $line-e (eq $line)))
    (type $section (record (field "heading" string) (field "lines" (list $line-e))))
    (export "section" (type $section-e (eq $section)))
    (type $panel (record (field "title" string) (field "sections" (list $section-e))))
    (export "panel" (type (eq $panel)))
  ))
  (import "power:panel/types@0.1.0" (instance $types (type $panel-types)))
  (alias export $types "command" (type $command))
  (alias export $types "effect" (type $effect))
  (alias export $types "overview" (type $overview))
  (alias export $types "panel" (type $panel))

  (core module $plugin
    (memory (export "memory") 1)
    ;; commands(): the return area at 0 points at one command at 64.
    (data (i32.const 0) "\40\00\00\00\01\00\00\00")
    ;; command: id, title, description, scope = box, default-keys
    (data (i32.const 64)
      "\00\02\00\00\0c\00\00\00"   ;; id -> 512 "fixture.open"
      "\10\02\00\00\0c\00\00\00"   ;; title -> 528 "Open fixture"
      "\20\02\00\00\16\00\00\00"   ;; description -> 544 "Open the fixture panel"
      "\00\00\00\00"               ;; scope box
      "\80\00\00\00\01\00\00\00")  ;; default-keys -> 128, 1 key
    (data (i32.const 128) "\40\02\00\00\01\00\00\00")  ;; "o" at 576
    ;; invoke(): ok(open-panel("fixture")) at 160.
    (data (i32.const 160)
      "\00\00\00\00"               ;; ok
      "\01\00\00\00"               ;; open-panel
      "\48\02\00\00\07\00\00\00")  ;; "fixture" at 584
    ;; render-panel(): ok(panel) at 192.
    (data (i32.const 192)
      "\00\00\00\00"
      "\48\02\00\00\07\00\00\00"   ;; title "fixture"
      "\00\01\00\00\01\00\00\00")  ;; sections -> 256, 1
    ;; section at 256: heading "Totals" at 592, lines -> 288, 1
    (data (i32.const 256) "\50\02\00\00\06\00\00\00\20\01\00\00\01\00\00\00")
    ;; line at 288: left -> 320 (1 item), right -> 368 (1 item)
    (data (i32.const 288) "\40\01\00\00\01\00\00\00\70\01\00\00\01\00\00\00")
    ;; item at 320: text(segment "terminals" at 600, fg "accent" at 616, no bg, bold)
    (data (i32.const 320)
      "\00\00\00\00"
      "\58\02\00\00\09\00\00\00"
      "\01\00\00\00\68\02\00\00\06\00\00\00"
      "\00\00\00\00\00\00\00\00\00\00\00\00"
      "\01\00\00\00")
    ;; item at 368: bar(ratio, width set by render, fg "accent", no bg)
    (data (i32.const 368)
      "\01\00\00\00"
      "\00\00\00\00\00\00\00\00"
      "\01\00\00\00\68\02\00\00\06\00\00\00"
      "\00\00\00\00\00\00\00\00\00\00\00\00")
    (data (i32.const 512) "fixture.open")
    (data (i32.const 528) "Open fixture")
    (data (i32.const 544) "Open the fixture panel")
    (data (i32.const 576) "o")
    (data (i32.const 584) "fixture")
    (data (i32.const 592) "Totals")
    (data (i32.const 600) "terminals")
    (data (i32.const 616) "accent")
    ;; settings(): some({key "fixture", defaults-json at 704}) at 640.
    (data (i32.const 640) "\01\00\00\00\48\02\00\00\07\00\00\00\c0\02\00\00\13\00\00\00")
    (data (i32.const 704) "{\"refresh_ms\":2000}")
    ;; configure(): ok(some(refresh)) at 736, the refresh set per call.
    (data (i32.const 736) "\00\00\00\00\01\00\00\00\00\00\00\00")
    (global $next (mut i32) (i32.const 4096))
    (func (export "realloc") (param i32 i32 i32) (param $size i32) (result i32)
      (local $start i32)
      (if (i32.gt_u (global.get $next) (i32.const 60000))
        (then (global.set $next (i32.const 4096))))
      (local.set $start (global.get $next))
      (global.set $next (i32.and (i32.add (i32.add (local.get $start) (local.get $size)) (i32.const 7)) (i32.const -8)))
      (local.get $start)
    )
    (func (export "commands") (result i32) (i32.const 0))
    (func (export "settings") (result i32) (i32.const 640))
    (func (export "configure") (param $config i32) (param $length i32) (result i32)
      (i32.store (i32.const 744) (i32.mul (local.get $length) (i32.const 100)))
      (i32.const 736))
    (func (export "invoke") (param i32 i32) (result i32) (i32.const 160))
    ;; Parameters: panel-id at 0, overview at 8 (totals.terminals at 60, attached at 64), width at 80.
    (func (export "render") (param $args i32) (result i32)
      (local $terminals i32)
      (local.set $terminals (i32.load offset=60 (local.get $args)))
      (f32.store (i32.const 372)
        (if (result f32) (local.get $terminals)
          (then (f32.div
            (f32.convert_i32_u (i32.load offset=64 (local.get $args)))
            (f32.convert_i32_u (local.get $terminals))))
          (else (f32.const 0))))
      (i32.store (i32.const 376) (i32.load offset=80 (local.get $args)))
      (i32.const 192)
    )
  )
  (core instance $instance (instantiate $plugin))
  (alias core export $instance "memory" (core memory $memory))
  (alias core export $instance "realloc" (core func $realloc))
  (alias core export $instance "commands" (core func $commands))
  (alias core export $instance "settings" (core func $settings))
  (alias core export $instance "configure" (core func $configure))
  (alias core export $instance "invoke" (core func $invoke))
  (alias core export $instance "render" (core func $render))
  (func (export "settings") (result (option $plugin-settings))
    (canon lift (core func $settings) (memory $memory) (realloc $realloc)))
  (func (export "configure") (param "config-json" string) (result (result (option u32) (error string)))
    (canon lift (core func $configure) (memory $memory) (realloc $realloc)))
  (func (export "commands") (result (list $command))
    (canon lift (core func $commands) (memory $memory) (realloc $realloc)))
  (func (export "invoke") (param "id" string) (result (result $effect (error string)))
    (canon lift (core func $invoke) (memory $memory) (realloc $realloc)))
  (func (export "render-panel")
    (param "panel-id" string) (param "data" $overview) (param "width-cols" u32)
    (result (result $panel (error string)))
    (canon lift (core func $render) (memory $memory) (realloc $realloc)))
)
