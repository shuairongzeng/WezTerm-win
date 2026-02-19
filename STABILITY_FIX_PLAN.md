# WezTerm Windows 稳定性修复计划

> 生成日期: 2026-02-09
> 目标: 确保 WezTerm 在 Windows 10/11 上流畅运行
> 涉及任务: wezterm-xka (TCP Keep-Alive), wezterm-bgu (Socket Path Discovery) 及关联代码

---

## 修复任务总览

| ID | 优先级 | 状态 | 标题 | 文件 |
|----|--------|------|------|------|
| F01 | P0 | [x] | InputQueue 添加 timer 回退重试，防止死锁 | `termwindow/mod.rs` |
| F02 | P0 | [x] | do_recovery 检查 pane 存活，防止无限 recovery 循环 | `mux/src/lib.rs` |
| F03 | P0 | [x] | 粘贴操作捕获 WouldBlock 并入队重试 | `termwindow/clipboard.rs`, `termwindow/mod.rs` |
| F04 | P0 | [x] | 鼠标事件捕获 WouldBlock 并入队重试 | `termwindow/mouseevent.rs`, `termwindow/mod.rs` |
| F05 | P1 | [x] | Socket Listener 非白名单错误加 sleep 防 CPU 空转 | `local.rs` |
| F06 | P1 | [x] | Socket Listener 添加总 recovery 次数上限 | `local.rs` |
| F07 | P1 | [x] | NamedMutex::with_lock 处理 WAIT_ABANDONED | `discovery.rs` |
| F08 | P1 | [x] | Keep-Alive 设置失败时记录 warn 日志 | `windows.rs` |
| F09 | P1 | [x] | perform_actions sleep 时间从 ms 降到 us 级 | `localpane.rs` |
| F10 | P1 | [x] | 添加缺失的 Windows 错误码到白名单 | `local.rs` |
| F11 | P2 | [x] | INPUT_QUEUE_CAPACITY 从 32 增加到 128 | `termwindow/mod.rs` |
| F12 | P2 | [x] | dead 标志改用 Acquire/Release ordering | `mux/src/lib.rs` |
| F13 | P2 | [x] | flush() 改为 no-op（背景线程已自动 flush） | `domain.rs` |
| F14 | P2 | [x] | 共享内存写入前添加 MAX_NAME 边界检查 | `discovery.rs` |
| F15 | P2 | [x] | resolve() 路径校验防止 path traversal | `discovery.rs` |
| F16 | P2 | [x] | 错误信息修正 "creating" → "opening" | `discovery.rs` |
| F17 | P2 | [x] | 移除未使用的 reader_dead 标志 | `mux/src/lib.rs` |
| F18 | P2 | [x] | 消除不必要的 buffer copy | `mux/src/lib.rs` |

---

## 详细修复方案

### F01: InputQueue 添加 timer 回退重试 [P0-CRITICAL]

**问题**: `flush_pending_input()` 仅在 PaneOutput 事件触发时调用。当进程等待输入才产生输出时，
PaneOutput 永不到来 → pending_input 永不消费 → **死锁**。

**修复方案**:
- 在 `queue_input_op()` 中，当 queue 从空变为非空时，启动一个 50ms 定时重试
- 使用 `window.notify()` 或 `context.request_timer()` 触发定时回调
- 在定时回调中调用 `flush_pending_input()`
- 如果 flush 后 queue 仍非空，继续安排下一次定时

**修改文件**: `wezterm-gui/src/termwindow/mod.rs`
**影响行号**: 1540-1549 (queue_input_op), 1474-1536 (flush_pending_input)

---

### F02: do_recovery 检查 pane 存活 [P0-CRITICAL]

**问题**: `do_recovery()` 不检查 pane 是否已从 mux 移除。如果 pane 已死但 PTY reader 持续
产生数据 → 无限创建新 parser 线程。

**修复方案**:
- 在 `do_recovery()` 闭包开头添加 `pane.upgrade().is_none()` 检查
- 如果 pane 不存在，返回 `false` 终止 recovery

**修改文件**: `mux/src/lib.rs`
**影响行号**: 389-418 (do_recovery closure)

---

### F03: 粘贴操作 WouldBlock 处理 [P0-HIGH]

**问题**: `clipboard.rs:52` 使用 `.ok()` 静默丢弃所有错误，包括 WouldBlock。
用户粘贴内容完全丢失且无反馈。

**修复方案**:
- 在 `InputOp` 枚举中添加 `Paste(String)` 变体
- 修改 `paste_from_clipboard()` 捕获 WouldBlock 并入队
- 在 `flush_pending_input()` 中添加 Paste 处理分支

**修改文件**:
- `wezterm-gui/src/termwindow/mod.rs` (InputOp, flush_pending_input)
- `wezterm-gui/src/termwindow/clipboard.rs` (paste_from_clipboard)

---

### F04: 鼠标事件 WouldBlock 处理 [P0-HIGH]

**问题**: `mouseevent.rs:1035` 使用 `.ok()` 静默丢弃 WouldBlock。
vim/tmux 等 mouse-aware 应用中鼠标操作丢失。

**修复方案**:
- 在 `InputOp` 枚举中添加 `MouseEvent(...)` 变体
- 修改 `mouse_event_terminal()` 捕获 WouldBlock 并入队
- 在 `flush_pending_input()` 中添加 MouseEvent 处理分支

**修改文件**:
- `wezterm-gui/src/termwindow/mod.rs` (InputOp, flush_pending_input)
- `wezterm-gui/src/termwindow/mouseevent.rs` (mouse_event_terminal)

---

### F05: Socket Listener CPU 空转修复 [P1]

**问题**: 非白名单错误码 + `should_recover=false` 时，循环无 sleep，
10 轮迭代 CPU 空转。

**修复方案**:
- 在 recovery 判断后、`should_recover=false` 的路径上添加 `sleep(500ms)`

**修改文件**: `wezterm-mux-server-impl/src/local.rs`
**影响行号**: 113-136

---

### F06: Socket Listener 总 recovery 上限 [P1]

**问题**: `try_recover()` 成功后 `consecutive_errors` 归零。持久性问题导致
反复 recover → 无限循环。

**修复方案**:
- 添加 `total_recoveries` 计数器（不重置）
- 超过 5 次总 recovery 后退出

**修改文件**: `wezterm-mux-server-impl/src/local.rs`
**影响行号**: 53-165 (run)

---

### F07: NamedMutex 处理 WAIT_ABANDONED [P1]

**问题**: `WaitForSingleObject` 返回 `WAIT_ABANDONED` (0x80) 时代码视为错误。
实际上 `WAIT_ABANDONED` 仍然获取了 mutex 所有权。GUI 崩溃后 CLI 永久无法连接。

**修复方案**:
- 接受 `WAIT_ABANDONED` 作为成功获取（加 warn 日志）

**修改文件**: `wezterm-client/src/discovery.rs`
**影响行号**: 137-149 (with_lock)

---

### F08: Keep-Alive 失败日志 [P1]

**问题**: `let _ =` 静默忽略 WSAIoctl 失败。如果 Keep-Alive 未生效，
5 分钟超时问题会复现，但完全没有诊断信息。

**修复方案**:
- 检查 `configure_keepalive()` 返回值
- 失败时 `log::warn!` 记录 WSA 错误码

**修改文件**: `filedescriptor/src/windows.rs`
**影响行号**: 551-592

---

### F09: perform_actions sleep 降级 [P1]

**问题**: `sleep(2ms)` × 多个 chunk = 大量人为延迟。高吞吐时输出卡顿。

**修复方案**:
- 将 `sleep(Duration::from_millis(2))` 降为 `sleep(Duration::from_micros(200))`
- 将 `sleep(Duration::from_millis(1))` 降为 `sleep(Duration::from_micros(100))`

**修改文件**: `mux/src/localpane.rs`
**影响行号**: 396-438

---

### F10: 添加缺失 Windows 错误码 [P1]

**问题**: Socket Listener 白名单缺少 WSAEINTR(10004) 和 WSAEWOULDBLOCK(10035)。
这些是临时错误，不需要重建 socket，只需简单重试。

**修复方案**:
- 添加 10004, 10035 到白名单
- 区分"仅重试"和"需重建 socket"两种恢复策略

**修改文件**: `wezterm-mux-server-impl/src/local.rs`
**影响行号**: 113-116

---

### F11: INPUT_QUEUE_CAPACITY 扩容 [P2]

**修复**: 从 32 增到 128

**修改文件**: `wezterm-gui/src/termwindow/mod.rs` (line 212)

---

### F12: dead 标志 Ordering 修复 [P2]

**修复**: `Relaxed` → `Release`(store) / `Acquire`(load)

**修改文件**: `mux/src/lib.rs` (lines 143, 458, 555)

---

### F13: flush() 语义修复 [P2]

**修复**: flush() 通过 channel 发送 sentinel，等待背景线程实际 flush

**修改文件**: `mux/src/domain.rs` (lines 615-619)

---

### F14: 共享内存写入边界检查 [P2]

**修复**: 写入前检查 `path.len() < MAX_NAME`

**修改文件**: `wezterm-client/src/discovery.rs` (lines 202-205)

---

### F15: resolve() 路径校验 [P2]

**修复**: 检查路径不含 `..` 组件

**修改文件**: `wezterm-client/src/discovery.rs` (lines 235-239)

---

### F16: 错误信息修正 [P2]

**修复**: `"creating shared memory"` → `"opening shared memory"`

**修改文件**: `wezterm-client/src/discovery.rs` (line 85)

---

### F17: 移除 reader_dead 死代码 [P2]

**修复**: 删除 `reader_dead` 和 `reader_dead_clone`

**修改文件**: `mux/src/lib.rs` (line 341)

---

### F18: 消除多余 buffer copy [P2]

**修复**: `data_rx` 收到 data 后直接 `tx.write_all(&data)` 而非先 copy 到 buf

**修改文件**: `mux/src/lib.rs` (lines 470-475)

---

## 修复日志

| 时间 | 任务 | 状态 | 备注 |
|------|------|------|------|
| 2026-02-09 | 计划制定 | Done | 共 18 项修复 |
| 2026-02-09 | F02 | Done | do_recovery 添加 pane 存活检查，防止无限 recovery 循环 |
| 2026-02-09 | F08 | Done | Keep-Alive WSAIoctl 失败时 eprintln 警告（filedescriptor 无 log 依赖） |
| 2026-02-09 | F07 | Done | with_lock 接受 WAIT_ABANDONED，GUI 崩溃后 CLI 不再永久失败 |
| 2026-02-09 | F05+F06+F10 | Done | Listener: 错误码分类(retry/rebuild)、CPU 空转修复、总 recovery 上限 20 |
| 2026-02-09 | F09 | Done | perform_actions sleep 从 1-2ms 降到 100-200us |
| 2026-02-09 | F12 | Done | dead 标志 Relaxed → Release/Acquire |
| 2026-02-09 | F14+F15+F16 | Done | 共享内存边界检查、路径遍历防护、错误信息修正 |
| 2026-02-09 | F17+F18 | Done | 移除 reader_dead 死代码、消除 buf copy |
| 2026-02-09 | F03+F04+F11 | Done | InputOp 添加 Paste/MouseEvent 变体，容量 32→128 |
| 2026-02-09 | F01 | Done | queue_input_op 添加 50ms timer 回退重试，防止死锁 |
| 2026-02-09 | F13 | Done | flush() 改为 no-op，背景线程已自动 flush |
| 2026-02-09 | 编译验证 | Done | cargo check 通过，0 error，2 pre-existing warnings |

## 修改文件汇总

| 文件 | 修复项 | 改动概述 |
|------|--------|----------|
| `mux/src/lib.rs` | F02,F12,F17,F18 | recovery 存活检查、Ordering 修正、清理死代码、消除 buf copy |
| `mux/src/domain.rs` | F13 | flush() 改为 no-op |
| `mux/src/localpane.rs` | F09 | sleep 从 ms 降到 us |
| `filedescriptor/src/windows.rs` | F08 | Keep-Alive 失败时打印警告 |
| `wezterm-client/src/discovery.rs` | F07,F14,F15,F16 | WAIT_ABANDONED、边界检查、路径校验、错误信息 |
| `wezterm-mux-server-impl/src/local.rs` | F05,F06,F10 | 错误分类、CPU 防空转、recovery 上限 |
| `wezterm-gui/src/termwindow/mod.rs` | F01,F03,F04,F11 | InputOp 扩展、timer 重试、容量扩容 |
| `wezterm-gui/src/termwindow/clipboard.rs` | F03 | 粘贴 WouldBlock 处理 |
| `wezterm-gui/src/termwindow/mouseevent.rs` | F04 | 鼠标 WouldBlock 处理 |

