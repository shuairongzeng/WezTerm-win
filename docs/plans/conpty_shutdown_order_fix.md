# 修复 ConPTY 关闭顺序导致 pwsh.exe 崩溃

## 问题描述

在 WezTerm 中运行 opencode 后退出，pwsh.exe 崩溃并弹出"unknown hard error"系统警告。
Exit code `2148734499` = `0x80131623` = .NET `COR_E_FAILFAST`。

## 根因分析

WezTerm 的 ConPTY 资源释放顺序不正确。

在 `pty/src/win/conpty.rs` 中 `Inner` 结构体字段声明顺序为：
```rust
struct Inner {
    con: PsuedoCon,                    // 1st drop → ClosePseudoConsole
    readable: FileDescriptor,          // 2nd drop
    writable: Option<FileDescriptor>,  // 3rd drop
    size: PtySize,
}
```

Rust 按字段声明顺序执行 Drop。导致实际关闭顺序为：
1. `ClosePseudoConsole` → 向子进程发送 CTRL_CLOSE_EVENT
2. 关闭 stdout pipe (readable)
3. 关闭 stdin pipe (writable)

**Microsoft 文档要求的正确顺序**：
1. **先关闭 stdin pipe (writable)** — 通知子进程输入已结束，鼓励其退出
2. **再 ClosePseudoConsole** — 发送关闭信号
3. **最后关闭 stdout pipe (readable)** — 确保所有输出被处理

错误的顺序导致 `ClosePseudoConsole` 在 stdin 仍然打开时执行，pwsh.exe 收到 CTRL_CLOSE_EVENT 但仍持有有效的 stdin 句柄，
.NET CLR 在处理这种不一致状态时触发 FailFast。

## 修复方案

### [MODIFY] [conpty.rs](file:///e:/980BAK/git/wezterm/pty/src/win/conpty.rs)

为 `Inner` 实现自定义 `Drop`，按正确顺序释放资源：

```rust
impl Drop for Inner {
    fn drop(&mut self) {
        // Microsoft ConPTY shutdown sequence:
        // 1. Close stdin pipe first to signal EOF to child process
        self.writable.take();
        // 2. ClosePseudoConsole (handled by PsuedoCon::drop)
        //    This sends CTRL_CLOSE_EVENT to client processes
        // 3. stdout pipe (readable) closed automatically after con
        //    to drain remaining output
    }
}
```

同时调换字段声明顺序，确保 `con` 在 `readable` 之前、`writable` 之后 Drop：

```rust
struct Inner {
    writable: Option<FileDescriptor>,  // 1st drop: close stdin
    con: PsuedoCon,                    // 2nd drop: ClosePseudoConsole
    readable: FileDescriptor,          // 3rd drop: close stdout
    size: PtySize,
}
```

## 验证计划

1. `cargo check -p pty` 确认编译通过
2. `cargo build --release -p wezterm-gui` 编译完整版本
3. 复制到 dist 目录，运行 opencode → `/exit` 测试是否还崩溃
