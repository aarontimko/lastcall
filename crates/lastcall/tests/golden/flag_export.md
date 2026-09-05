lastcall flag · lastcall · crates/lastcall-engine/src/ops.rs · hunk 2 of 3 · 2026-09-05T18:04:00Z
note: why is this unwrap safe? the caller can pass an empty slice

```diff
@@ -10,7 +10,8 @@
 let n = parse(s);
-    n.unwrap()
+    n.expect("parsed above")
 }
```

lastcall flag · lastcall · crates/lastcall/src/tui/render.rs · 2026-09-05T18:04:00Z
note: this whole file is generated — don't hand-edit
