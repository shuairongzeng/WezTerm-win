# 修复 WezTerm 启动白屏/闪烁问题

## 问题描述

WezTerm 在 Windows 10+ 系统上启动时，会出现 100-300ms 的白屏闪烁，之后才恢复正常终端渲染。

## 根因分析

启动时序如下：

```
CreateWindowExW (不可见) → OpenGL 初始化 → window.show()（异步）→ ShowWindow → WM_PAINT → NeedRepaint → do_paint
```

白屏出现在 **`ShowWindow` 到首帧 `do_paint` 完成**之间。具体原因有两个：

### 原因 1：wm_paint 首帧不做实际绘制

[wm_paint](file:///e:/980BAK/git/wezterm/window/src/os/windows/window.rs#L1619-L1682) 在 `BeginPaint/EndPaint` 之间什么都不做（注释写着 "Do nothing right now"），仅异步 `dispatch(NeedRepaint)` 让应用层在下一次事件循环中渲染。这导致 `ShowWindow` 触发的首次 `WM_PAINT` 时，窗口内容区域完全为空。

### 原因 2：窗口类无背景画刷

[create_window](file:///e:/980BAK/git/wezterm/window/src/os/windows/window.rs#L436) 中设置 `hbrBackground: null_mut()`，窗口类没有默认背景画刷。虽然 `WM_ERASEBKGND` 返回 `Some(1)` 阻止了 Windows 默认擦除，但在无内容渲染的首帧期间，窗口区域显示为 Windows 默认的白色背景。

## 修复方案

在 `wm_paint` 首帧（OpenGL 已就绪但尚未渲染）时，使用 GDI 将窗口区域填充为黑色，避免白色闪烁。同时设置窗口类的背景画刷为黑色，从根本上保证窗口在任何时刻的默认绘制都是深色。

---

### 窗口层 (window crate)

#### [MODIFY] [window.rs](file:///e:/980BAK/git/wezterm/window/src/os/windows/window.rs)

**改动 1：设置黑色背景画刷**

在 `create_window` 函数中，将 `hbrBackground: null_mut()` 改为使用 `GetStockObject(BLACK_BRUSH)` 获取的系统黑色画刷。

```diff
-            hbrBackground: null_mut(),
+            hbrBackground: unsafe { GetStockObject(BLACK_BRUSH) } as HBRUSH,
```

需新增引用：`use winapi::um::wingdi::{GetStockObject, BLACK_BRUSH};` 和 `use winapi::shared::windef::HBRUSH;`

**改动 2：wm_paint 首帧 GDI 填充**

在 `wm_paint` 函数中，在 `BeginPaint/EndPaint` 之间添加逻辑：如果 OpenGL 尚未完成首帧渲染（`gl_state` 为 `None`），则使用 `FillRect` + `GetStockObject(BLACK_BRUSH)` 将窗口区域填充为黑色。

```diff
     let _ = BeginPaint(hwnd, &mut ps);
-    // Do nothing right now
+    // If OpenGL is not yet initialized, fill the window with black
+    // to prevent white flash during startup
+    if inner.gl_state.is_none() {
+        FillRect(ps.hdc, &ps.rcPaint, GetStockObject(BLACK_BRUSH) as HBRUSH);
+    }
     EndPaint(hwnd, &mut ps);
```

## 验证计划

### 手动验证

1. 在项目根目录执行 `cargo build -p wezterm-gui`
2. 运行编译后的 `wezterm-gui.exe`
3. 观察启动时是否还有白色闪屏现象
4. 预期结果：窗口出现时直接显示深色/黑色背景，无白色闪烁
5. 功能验证：确认终端正常工作，文字显示、颜色、透明度等均无异常
