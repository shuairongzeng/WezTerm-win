# WezTerm Windows 稳定性综合修复方案

## 背景

当前 WezTerm 在 Windows 上运行时存在多个稳定性问题，表现为：退出后报错、pane 假死必须重启、孤儿进程残留、以及长时间运行后的各种异常。本方案基于对 Windows 平台核心代码的深度审计，定位了 **8 个仍存在的根因问题**。

> [!NOTE]
> 此前已有部分修复（如 `d6782369b` subscriber 计数器、`ffa91f498` TCP→crossbeam、`2975961c1` ConPTY 字段顺序调整），本方案针对**当前代码中仍未修复**的问题。

---

## 问题总览

| # | 问题 | 文件 | 优先级 | 症状关联 |
|---|------|------|--------|----------|
| 1 | ConPTY `Inner` 缺少显式自定义 Drop | `pty/src/win/conpty.rs` | 🔴 高 | 退出时报错（.NET FailFast） |
| 2 | `LocalPane::drop` 只 kill 不 wait | `mux/src/localpane.rs` | 🔴 高 | 孤儿 OpenConsole.exe 进程 |
| 3 | `GetExitCodeProcess` 失败静默处理 | `pty/src/win/mod.rs` | 🟡 中高 | pane 状态机卡住 |
| 4 | `WaitForSingleObject` 使用 `INFINITE` | `pty/src/win/mod.rs` | 🟡 中高 | 后台线程永久泄漏 |
| 5 | `pane_state()` 隐式插入空状态 | `wezterm-gui/src/termwindow/mod.rs` | 🟡 中 | 内存泄漏、状态污染 |
| 6 | `Mux::shutdown()` 清理不彻底 | `mux/src/lib.rs` | 🟡 中 | 退出时子进程未清理、报错 |
| 7 | Recovery 达到上限后 pane 静默死亡 | `mux/src/lib.rs` | 🟢 中低 | 必须手动关闭 pane |
| 8 | `window_miss_count` 重置不完整 | `wezterm-gui/src/termwindow/mod.rs` | 🟢 低 | 边缘场景订阅过早取消 |

---

## 问题 1：ConPTY `Inner` 缺少显式自定义 Drop

### 位置
`pty/src/win/conpty.rs` 第 48-60 行

### 当前代码
```rust
struct Inner {
    // Field order matters for Drop: Rust drops fields in declaration order.
    // Microsoft ConPTY shutdown sequence requires:
    //   1. Close stdin pipe (writable)
    //   2. ClosePseudoConsole (con)
    //   3. Close stdout pipe (readable)
    // Wrong order causes child processes (e.g. pwsh.exe/.NET) to crash
    // with FailFast (0x80131623) due to inconsistent handle state.
    writable: Option<FileDescriptor>,
    con: PsuedoCon,
    readable: FileDescriptor,
    size: PtySize,
}
```

### 根因
虽然字段声明顺序已调整为正确的 Drop 顺序（`writable` → `con` → `readable`），但**代码仅依赖 Rust 的隐式字段顺序 Drop**，没有显式自定义 `impl Drop for Inner`。这意味着：
- 未来任何重构（如添加新字段、调整字段顺序）都可能无意中破坏关闭顺序
- 代码意图不够明确，新贡献者难以意识到字段顺序的语义重要性
- `plan` 文档（`docs/plans/conpty_shutdown_order_fix.md`）中已明确建议实现自定义 Drop，但代码中未实施

### 影响
退出 wezterm 或关闭 pane 时，pwsh.exe / .NET 程序可能崩溃并弹出"unknown hard error"，exit code `0x80131623`（`COR_E_FAILFAST`）。

### 修复方案
为 `Inner` 实现显式自定义 Drop，并保留字段顺序作为双重保险：

```rust
impl Drop for Inner {
    fn drop(&mut self) {
        // Microsoft ConPTY shutdown sequence:
        // 1. Close stdin pipe first to signal EOF to child process
        //    This encourages the child to gracefully exit.
        self.writable.take();
        // 2. ClosePseudoConsole (handled by PsuedoCon::drop)
        //    This sends CTRL_CLOSE_EVENT to client processes.
        //    Must happen AFTER stdin is closed to avoid inconsistent
        //    handle state that causes .NET FailFast (0x80131623).
        // 3. stdout pipe (readable) is closed automatically after `con`
        //    because `readable` is declared after `con` in the struct.
        //    This ensures all remaining output is drained.
    }
}
```

> [!NOTE]
> 自定义 Drop 中不显式 `drop(self.con)` 或 `drop(self.readable)`，因为 Rust 在自定义 Drop 之后仍会按字段顺序执行各字段的析构。我们只需在自定义 Drop 中提前 `take()` 掉 `writable`，确保它在 `con` 之前被释放。

---

## 问题 2：`LocalPane::drop` 只 kill 不 wait

### 位置
`mux/src/localpane.rs` 第 1234-1241 行

### 当前代码
```rust
impl Drop for LocalPane {
    fn drop(&mut self) {
        // Avoid lingering zombies if we can, but don't block forever.
        // <https://github.com/wezterm/wezterm/issues/558>
        if let ProcessState::Running { signaller, .. } = &mut *self.process.lock() {
            let _ = signaller.kill();
        }
    }
}
```

### 根因
`Drop` 中调用 `signaller.kill()`（即 `TerminateProcess`）后立即返回，**不等待进程实际终止**。在 Windows 上，`TerminateProcess` 是异步的——系统标记进程为终止状态，但进程可能还需要几毫秒到几十毫秒才能真正退出并释放资源（如关闭句柄、终止线程）。

同时，`split_child` 中启动了一个后台线程调用 `process.wait()`，该线程在 `LocalPane` drop 后仍在运行，因为它没有被 join。

### 影响
- 快速创建/销毁 pane 时，可能留下孤儿 `OpenConsole.exe` 或 `conhost.exe` 进程
- 这些孤儿进程持有 ConPTY 句柄，可能导致后续新 pane 无法正确初始化
- 必须关闭整个 wezterm 窗口才能清理这些孤儿进程

### 修复方案
在 `Drop` 中增加一个有限时间的 wait（非阻塞但给予清理时间窗口）：

```rust
impl Drop for LocalPane {
    fn drop(&mut self) {
        // Avoid lingering zombies if we can, but don't block forever.
        // <https://github.com/wezterm/wezterm/issues/558>
        if let ProcessState::Running { signaller, child_waiter, .. } = &mut *self.process.lock() {
            let _ = signaller.kill();
            // On Windows, TerminateProcess is asynchronous. Give the child
            // a brief window to actually terminate and release handles,
            // otherwise OpenConsole.exe may linger and interfere with
            // subsequent pane creation.
            #[cfg(windows)]
            {
                use std::time::Duration;
                // Non-blocking check: if the child_waiter already has
                // a result, we know it's done. Otherwise we don't wait.
                if let Ok(Some(_)) = child_waiter.try_recv() {
                    // Process already exited, nothing more to do
                } else {
                    // Yield briefly to allow the OS to schedule the
                    // terminated process for cleanup. This is NOT a
                    // blocking wait; it's a cooperative yield.
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
    }
}
```

> [!IMPORTANT]
> 不阻塞等待是因为 `Drop` 不应做长时间阻塞操作。50ms 是一个经验值，足够 OS 完成大多数进程的终止清理，又不会影响用户体验。

---

## 问题 3：`GetExitCodeProcess` 失败静默处理

### 位置
`pty/src/win/mod.rs` 第 30-47 行

### 当前代码
```rust
fn is_complete(&mut self) -> IoResult<Option<ExitStatus>> {
    let mut status: DWORD = 0;
    let proc = self.proc.lock().unwrap().try_clone().unwrap();
    let res = unsafe { GetExitCodeProcess(proc.as_raw_handle() as _, &mut status) };
    if res != 0 {
        if status == STILL_ACTIVE {
            Ok(None)
        } else {
            Ok(Some(ExitStatus::with_exit_code(status)))
        }
    } else {
        // Log the error but return None to allow retry
        // This can happen if the process handle becomes invalid
        let err = IoError::last_os_error();
        log::warn!("GetExitCodeProcess failed: {:?}", err);
        Ok(None)  // ← 静默返回 None
    }
}
```

### 根因
当 `GetExitCodeProcess` 失败时（如句柄已无效、进程已完全消失），代码记录 warning 但返回 `Ok(None)`，调用方会认为"进程仍在运行"。这可能导致：
- `LocalPane::is_dead()` 永远无法检测到进程已死亡
- pane 状态机永远卡在 `Running` 状态
- 用户看到 pane 不响应但 wezterm 认为它还在运行

### 影响
在 Windows 上某些极端情况下（如系统内存压力、句柄表损坏），pane 实际已退出但 wezterm 认为它仍在运行，导致 pane 假死。

### 修复方案
区分"句柄无效"和"其他错误"：

```rust
fn is_complete(&mut self) -> IoResult<Option<ExitStatus>> {
    let mut status: DWORD = 0;
    let proc = self.proc.lock().unwrap().try_clone().unwrap();
    let res = unsafe { GetExitCodeProcess(proc.as_raw_handle() as _, &mut status) };
    if res != 0 {
        if status == STILL_ACTIVE {
            Ok(None)
        } else {
            Ok(Some(ExitStatus::with_exit_code(status)))
        }
    } else {
        let err = IoError::last_os_error();
        match err.raw_os_error() {
            // ERROR_INVALID_HANDLE (6): The process handle is no longer valid,
            // which strongly implies the process has terminated and its handle
            // was already closed/reaped by the OS.
            Some(6) => {
                log::debug!("GetExitCodeProcess: handle invalid, assuming process exited");
                Ok(Some(ExitStatus::with_exit_code(1)))
            }
            // For other errors, we still return None but log at a higher level
            // to aid debugging.
            _ => {
                log::warn!("GetExitCodeProcess failed: {:?}", err);
                Ok(None)
            }
        }
    }
}
```

---

## 问题 4：`WaitForSingleObject` 使用 `INFINITE` 超时

### 位置
`pty/src/win/mod.rs` 第 105 行和第 153 行

### 当前代码
```rust
fn wait(&mut self) -> IoResult<ExitStatus> {
    // ...
    let wait_result = unsafe { WaitForSingleObject(proc.as_raw_handle() as _, INFINITE) };
    // ...
}

// In Future::poll:
std::thread::spawn(move || {
    let result = unsafe { WaitForSingleObject(handle.0 as _, INFINITE) };
    if result == WAIT_FAILED {
        log::warn!("WaitForSingleObject failed in poll(): {:?}", IoError::last_os_error());
    }
    waker.wake();
});
```

### 根因
`WaitForSingleObject` 使用 `INFINITE` 超时。在 Windows 上，如果进程句柄异常（如成为孤儿进程、内核对象损坏、驱动级问题），该调用可能**永久阻塞**。虽然 `waiter_spawned` AtomicBool 防止了重复创建线程，但已创建的线程可能永远挂在那里。

### 影响
- 后台 waiter 线程泄漏
- 如果 `Future::poll` 中的 waiter 线程永久阻塞，waker 永远不会被调用，依赖该 Future 的代码可能永远等待
- 程序退出时这些线程成为"分离线程"，可能干扰正常关闭

### 修复方案
将 `INFINITE` 替换为合理的有限超时（30 秒），并在超时后重试有限次数：

```rust
// 在模块顶部添加常量
const WAIT_TIMEOUT_MS: DWORD = 30000; // 30 seconds
const MAX_WAIT_RETRIES: u32 = 3;

fn wait(&mut self) -> IoResult<ExitStatus> {
    if let Ok(Some(status)) = self.try_wait() {
        return Ok(status);
    }
    let proc = self.proc.lock().unwrap().try_clone().unwrap();
    
    for attempt in 1..=MAX_WAIT_RETRIES {
        let wait_result = unsafe {
            WaitForSingleObject(proc.as_raw_handle() as _, WAIT_TIMEOUT_MS)
        };
        match wait_result {
            WAIT_FAILED => {
                return Err(IoError::last_os_error());
            }
            winapi::um::winbase::WAIT_TIMEOUT => {
                log::warn!(
                    "WaitForSingleObject timed out (attempt {}/{})",
                    attempt, MAX_WAIT_RETRIES
                );
                if attempt == MAX_WAIT_RETRIES {
                    return Err(IoError::new(
                        std::io::ErrorKind::TimedOut,
                        "Process did not exit within the expected time"
                    ));
                }
                // Brief yield before retry to avoid spinning
                std::thread::sleep(Duration::from_millis(100));
            }
            // WAIT_OBJECT_0 or other success codes
            _ => {
                let mut status: DWORD = 0;
                let res = unsafe { GetExitCodeProcess(proc.as_raw_handle() as _, &mut status) };
                if res != 0 {
                    return Ok(ExitStatus::with_exit_code(status));
                } else {
                    return Err(IoError::last_os_error());
                }
            }
        }
    }
    
    unreachable!()
}

// In Future::poll:
if !self.waiter_spawned.swap(true, Ordering::SeqCst) {
    // ...
    std::thread::spawn(move || {
        let result = unsafe { WaitForSingleObject(handle.0 as _, WAIT_TIMEOUT_MS) };
        if result == WAIT_FAILED {
            log::warn!("WaitForSingleObject failed in poll(): {:?}", IoError::last_os_error());
        } else if result == winapi::um::winbase::WAIT_TIMEOUT {
            log::warn!("WaitForSingleObject timed out in poll()");
        }
        // Even on timeout/failure, wake the future so it can retry or error out
        waker.wake();
    });
}
```

---

## 问题 5：`pane_state()` 隐式插入空状态

### 位置
`wezterm-gui/src/termwindow/mod.rs` 第 3621-3625 行

### 当前代码
```rust
pub fn pane_state(&self, pane_id: PaneId) -> RefMut<'_, PaneState> {
    RefMut::map(self.pane_state.borrow_mut(), |state| {
        state.entry(pane_id).or_insert_with(PaneState::default)
    })
}
```

### 根因
`pane_state()` 是一个**看似只读实则写入**的 API。任何调用（即使是检查 viewport 这类纯查询操作）都会在 `HashMap` 中插入一个空的 `PaneState`。调用者包括：
- `flush_pending_input` → 合理（需要状态）
- `queue_input_op` → 合理（需要状态）
- `get_viewport` → **不合理**（纯查询）
- `scroll_to_bottom` → 合理（修改状态）
- 鼠标/键盘事件处理中的临时检查

### 影响
- 长期运行后，`pane_state` HashMap 中积累了大量已不存在 pane 的空状态条目
- 内存缓慢增长（虽然每个 `PaneState` 不大，但长期运行 + 频繁创建/关闭 pane 会累积）
- 在 `PaneRemoved` 处理时，`remove_runtime_state_for_pane` 会清理，但如果某些路径未触发移除通知，残留条目会持续存在

### 修复方案
添加一个**非插入式**的查询 API，并将只读场景迁移过去：

```rust
/// Non-inserting read-only access to pane state.
/// Returns None if no state exists for this pane.
pub fn try_pane_state(&self, pane_id: PaneId) -> Option<RefMut<'_, PaneState>> {
    RefMut::filter_map(self.pane_state.borrow_mut(), |state| {
        state.get_mut(&pane_id)
    }).ok()
}

/// Mutable access to pane state. Inserts default if not present.
pub fn pane_state(&self, pane_id: PaneId) -> RefMut<'_, PaneState> {
    RefMut::map(self.pane_state.borrow_mut(), |state| {
        state.entry(pane_id).or_insert_with(PaneState::default)
    })
}
```

迁移 `get_viewport` 等纯查询场景：
```rust
pub fn get_viewport(&self, pane_id: PaneId) -> Option<StableRowIndex> {
    self.try_pane_state(pane_id).and_then(|s| s.viewport)
}
```

同时，在 `PaneRemoved` 通知处理中确保总是调用 `remove_runtime_state_for_pane`。

---

## 问题 6：`Mux::shutdown()` 清理不彻底

### 位置
`mux/src/lib.rs` 第 1202-1204 行

### 当前代码
```rust
pub fn shutdown() {
    MUX.lock().take();
}
```

### 根因
`shutdown()` 只是将全局 `MUX` 实例 `take()` 掉，完全依赖 Rust 的 Drop 顺序进行级联清理。如果存在以下情况，清理可能不彻底：
- `Arc` 循环引用（如 pane → tab → window → pane）
- 后台线程仍持有 `Arc<Mux>` 或 `Arc<dyn Pane>` 引用
- 子进程（WinChild）在 `Mux` drop 之后才收到终止信号
- `LocalPane::drop` 中的 `signaller.kill()` 可能失败（如句柄已无效）

### 影响
- wezterm 退出时，后台的 `OpenConsole.exe`、用户子进程（pwsh、cmd）可能仍在运行
- 这些残留进程可能导致"必须关闭程序"的报错（因为 ConPTY 资源被占用，新实例无法启动）
- 在 Windows 上，未正确关闭的 ConPTY 会话可能导致系统级资源泄漏

### 修复方案
在 `take()` 之前，显式遍历并清理所有 pane：

```rust
pub fn shutdown() {
    // Explicitly clean up all panes before dropping the MUX.
    // This ensures child processes are signaled to terminate
    // and have a chance to clean up before we destroy the Mux.
    // Without this, Arc cycles or lingering thread references
    // may prevent proper cleanup on Windows.
    let pane_ids: Vec<PaneId> = {
        let mux_guard = MUX.lock();
        if let Some(mux) = mux_guard.as_ref() {
            mux.panes.read().keys().copied().collect()
        } else {
            return;
        }
    };
    
    for pane_id in pane_ids {
        let pane = {
            let mux_guard = MUX.lock();
            if let Some(mux) = mux_guard.as_ref() {
                mux.get_pane(pane_id)
            } else {
                None
            }
        };
        if let Some(pane) = pane {
            pane.kill();
        }
    }
    
    // Give processes a brief moment to react to kill signals
    #[cfg(windows)]
    std::thread::sleep(std::time::Duration::from_millis(100));
    
    MUX.lock().take();
}
```

---

## 问题 7：Recovery 达到上限后 pane 静默死亡

### 位置
`mux/src/lib.rs` 第 517-524 行（parser）和第 717-723 行（reader）

### 当前代码
```rust
if *recovery_count > MAX_PARSER_RECOVERIES {
    log::error!(
        "read_from_pane_pty: parser recovery limit ({}) reached for pane {}",
        MAX_PARSER_RECOVERIES,
        pane_id
    );
    return false;
}
```

### 根因
当 parser 崩溃 5 次或 reader 断开 3 次后，`read_from_pane_pty` 主循环退出，pane 的 PTY 读取彻底停止。此时 pane 对 wezterm 来说仍然是"存在"的，但不再接收任何输出。用户看到的是：可以输入但没有任何响应。

### 影响
- 在系统资源紧张、驱动问题、或 ConPTY 不稳定的 Windows 环境中，pane 可能在运行一段时间后彻底假死
- 用户必须手动关闭 pane 并重新创建
- 没有明显的错误提示告诉用户发生了什么

### 修复方案
达到上限后，主动触发 pane 关闭或显示错误信息：

```rust
if *recovery_count > MAX_PARSER_RECOVERIES {
    log::error!(
        "read_from_pane_pty: parser recovery limit ({}) reached for pane {}",
        MAX_PARSER_RECOVERIES,
        pane_id
    );
    // Don't let the pane silently die. Notify the mux to close it
    // so the user knows something went wrong and can recreate it.
    promise::spawn::spawn_into_main_thread(async move {
        if let Some(mux) = Mux::try_get() {
            if let Some(pane) = mux.get_pane(pane_id) {
                // Send an alert so the GUI can show a message
                pane.alert(Alert::SetUserVar {
                    name: "error".to_string(),
                    value: "Pane internal recovery limit exceeded. Please recreate this pane.".to_string(),
                });
            }
            // Optionally close the pane automatically after a delay
            // mux.remove_pane(pane_id);
        }
    }).detach();
    return false;
}
```

对于 reader recovery 上限同理。

---

## 问题 8：`window_miss_count` 重置不完整

### 位置
`wezterm-gui/src/termwindow/mod.rs` 第 1811-1815 行

### 当前代码
```rust
| MuxNotification::PaneAdded(_pane_id) => {
    let mux = Mux::get();
    return mux.get_window(mux_window_id).is_some();
}
```

### 根因
当 `PaneAdded` 通知到达时，代码检查 window 是否存在并返回布尔值，但**没有重置 `window_miss_count`**。虽然 `PaneAdded` 场景下 window 通常存在，miss 计数器不影响逻辑正确性，但如果 window 确实被短暂移除后重新添加（如 workspace 切换），计数器保持非零值可能导致后续更早触发取消订阅。

### 影响
- 边缘场景下，subscriber 可能比预期更早被取消
- 属于防御性修复，提升代码健壮性

### 修复方案
```rust
| MuxNotification::PaneAdded(_pane_id) => {
    let mux = Mux::get();
    let exists = mux.get_window(mux_window_id).is_some();
    if exists {
        window_miss_count.store(0, Ordering::Relaxed);
    }
    return exists;
}
```

---

## 修复实施优先级建议

| 优先级 | 问题 | 理由 |
|--------|------|------|
| P0 | 问题 1（ConPTY 显式 Drop） | 防回归，直接关联退出崩溃 |
| P0 | 问题 2（LocalPane drop wait） | 直接关联孤儿进程和"必须重启" |
| P1 | 问题 3（GetExitCodeProcess 错误） | 提升错误处理的健壮性 |
| P1 | 问题 4（INFINITE 超时） | 防止线程泄漏 |
| P1 | 问题 6（Mux shutdown） | 直接关联退出时子进程残留 |
| P2 | 问题 5（pane_state 隐式插入） | 内存泄漏和状态污染 |
| P2 | 问题 7（recovery 上限通知） | 改善用户体验 |
| P3 | 问题 8（miss_count 重置） | 防御性修复 |

---

## 验证计划

### 编译验证
```powershell
cargo check -p pty -p mux -p wezterm-gui
```

### 功能验证

| 测试项 | 步骤 | 预期结果 |
|--------|------|----------|
| 退出无崩溃 | 在 pwsh.exe pane 中运行程序后退出 wezterm | 无"unknown hard error"弹窗 |
| 孤儿进程清理 | 快速创建/关闭 10 个 pane，观察进程管理器 | 无残留 OpenConsole.exe |
| 长时间运行 | 运行 wezterm 超过 8 小时 | pane 正常工作，无静默死亡 |
| 假死恢复 | 在多个 pane 中同时运行大量输出程序 | 无"必须重启才能输入" |
| 正常关闭 | 关闭 wezterm 窗口 | 所有子进程（cmd/pwsh）同时终止 |

---

## 请审核确认

1. **是否同意按上述优先级实施？**
2. **问题 2 的 50ms sleep 是否可接受，还是你更倾向于其他方案（如尝试非阻塞 wait）？**
3. **问题 7 达到上限后是自动关闭 pane，还是仅显示错误消息让用户手动关闭？**
4. **是否需要先实施 P0/P1 问题，验证后再继续 P2/P3？**
