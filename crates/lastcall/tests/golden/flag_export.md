lastcall flag · lastcall · crates/lastcall-engine/src/ops.rs · hunk 2 of 3 · 2026-09-05T18:04:00Z
note: why is this unwrap safe? the caller can pass an empty slice

```diff
@@ -10,7 +10,8 @@
 let n = parse(s);
-    n.unwrap()
+    n.expect("parsed above")
 }
```

lastcall flag · lastcall · crates/lastcall/src/tui/render.rs · whole file · 2026-09-05T18:04:00Z
3 hunks · +12 −4
note: this whole file is generated — don't hand-edit

lastcall flag · lastcall · crates/lastcall/src/tui/keys.rs · hunk 3 of 3 · 2026-09-05T18:04:00Z
note: paste guard · ESC ^[ · DEL ^? · CSI ^[[ · BEL ^G

````diff
@@ -40,3 +40,3 @@ fn render()
-println!("x");
```
+println!("y");
````

lastcall flag · lastcall · crates/lastcall/src/tui/term.rs · whole file · 2026-09-05T18:04:00Z
note: a Phase 7 flag, before the summary existed
