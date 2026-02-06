# Roger Player - 技术文档

> 本文档面向 AI Agent 和开发者，说明项目架构、设计决策和开发注意事项。

## 项目定位

**极致音质的音频播放器**，专注于：
- 时序绝对稳定（低 jitter）
- Bit-perfect 数据传输
- 最短信号路径

**不是通用播放器**，不追求功能丰富，而是在 macOS + CoreAudio 约束下将软件层优化到极限。

---

## 架构概览

```
┌─────────────────────────────────────────────────────────────────┐
│                   TUI (终端界面，完全隔离)                         │
│  - 独立 malloc zone（堆内存隔离）                                  │
│  - 最低线程优先级 + 亲和性标签隔离                                   │
│  - 输入轮询与渲染解耦                                              │
└───────────┬─────────────────────────────────────────────────────┘
            │ Arc<Engine> (只读状态查询，无锁)
            ▼
┌─────────────────────────────────────────────────────────────────┐
│                         Engine (引擎层)                          │
│  - 整合解码、缓冲、输出                                            │
│  - 管理播放状态 (Stopped/Playing/Paused/Buffering)                │
└─────────────────────────────────────────────────────────────────┘
        │                                      │
        ▼                                      ▼
┌───────────────────┐              ┌───────────────────────────────┐
│   Decoder Thread  │              │    CoreAudio IO Thread        │
│   (解码线程)       │              │    (音频输出回调)              │
│                   │              │                               │
│ - symphonia 解码   │  Lock-free   │ - THREAD_TIME_CONSTRAINT      │
│ - i32 左对齐转换   │ ──────────→  │ - 零拷贝/格式转换              │
│ - NEON SIMD 加速  │  Ring Buffer │ - TPDF Dither (Float32)       │
│ - 亲和性标签 1     │   (SPSC)     │ - 亲和性标签 1                 │
└───────────────────┘              └───────────────────────────────┘
```

### 关键设计原则

1. **解码和输出完全解耦** - 通过 lock-free ring buffer 连接，互不阻塞
2. **IO 回调中禁止**：内存分配、加锁、系统调用、日志、诊断统计
3. **数据格式统一** - 全程使用 i32 左对齐，避免多次转换
4. **SRC 由 CoreAudio 处理** - 当源采样率与设备不匹配时，使用 CoreAudio 内置 SRC
5. **TUI 完全隔离** - TUI 的堆分配、线程调度、CPU 亲和性与音频线程完全分离

---

## 模块结构

```
src/
├── lib.rs              # 库入口，导出公共 API (audio, decode, engine)
├── main.rs             # CLI 入口 + 全局内存分配器注册
├── alloc.rs            # TUI 线程堆内存隔离（macOS malloc zone）
├── audio/
│   ├── mod.rs          # 音频模块导出
│   ├── output.rs       # CoreAudio 输出 (HALOutput/DefaultOutput + TPDF dither)
│   ├── ring_buffer.rs  # Lock-free SPSC 环形缓冲区
│   ├── stats.rs        # 播放统计（仅 samples_played + underrun_count）
│   ├── format.rs       # 音频格式定义和样本转换
│   └── timing.rs       # Mach 时间相关函数 (timebase 转换)
├── decode/
│   ├── mod.rs          # 解码模块导出
│   └── decoder.rs      # symphonia 解码器封装 + NEON SIMD 加速
├── engine/
│   └── mod.rs          # 播放引擎（状态管理、线程协调）
└── tui/
    ├── mod.rs          # TUI 模块导出
    ├── model.rs        # 应用状态模型（App struct）
    ├── view.rs         # 渲染逻辑（ratatui）
    └── controller.rs   # 事件循环 + 隔离措施初始化
```

---

## 核心组件详解

### 1. Ring Buffer (`audio/ring_buffer.rs`)

**职责**：解码线程和 IO 线程之间的数据传递

**关键特性**：
- **SPSC (Single-Producer Single-Consumer)** - 无需复杂同步
- **Wait-free** - 读写操作保证常数时间完成
- **CacheLine 对齐** - `#[repr(C, align(128))]` 避免 false sharing（Apple Silicon P-core 128 字节缓存行）
- **mlock 锁定** - 防止 page fault 导致时序抖动
- **2^n 容量** - 位与代替取模，快速索引计算

```rust
// 核心数据结构
pub struct RingBuffer<T: Copy + Default> {
    buffer: Box<[UnsafeCell<T>]>,
    capacity: usize,
    mask: usize,  // capacity - 1，用于快速取模
    write_pos: CacheLine<AtomicUsize>,  // 生产者位置（独占缓存行）
    read_pos: CacheLine<AtomicUsize>,   // 消费者位置（独占缓存行）
    memory_locked: AtomicBool,
}
```

**使用方式**：
- 解码线程调用 `write()` 写入样本
- IO 回调调用 `read()` 读取样本
- 两者可完全并发，无锁

### 2. 音频输出 (`audio/output.rs`)

**职责**：CoreAudio 设备管理和音频输出

**后端优先级**：
1. **HALOutput AudioUnit** - 绕过系统混音器，直接访问硬件设备（有线/USB）
2. **DefaultOutput AudioUnit** - 通过系统混音器，用于蓝牙等设备

**输出格式优先级**（HALOutput 模式）：
1. **Physical Format (Int32)** - 直接硬件访问，bit-perfect
2. **ASBD Integer (Int32/Int24)** - 通过 AudioUnit 整数输出
3. **Float32 + TPDF Dither** - 回退路径（DefaultOutput 默认使用）

**关键技术**：
- **AudioUnit 后端选择**
  - HALOutput: 直接硬件访问，绕过系统混音器，最佳音质
  - DefaultOutput: 通过系统混音器，兼容蓝牙设备
- **Hog Mode**: 独占设备，防止其他应用干扰（HALOutput 模式）
- **采样率智能选择**: 精确匹配 > 整数分频 > 最近值
- **IO 线程实时调度**: `THREAD_TIME_CONSTRAINT_POLICY`（首次回调时设置，period 基于实际设备 buffer_frames）
- **设备能力查询**: buffer size range, latency, safety offset
- **CoreAudio SRC**: 当源采样率与设备不匹配时，由 CoreAudio 自动处理采样率转换
- **TPDF Dither**: Float32 输出时使用 xorshift32 PRNG 生成三角形分布抖动
- **回调缓冲区精确分配**: `buffer_frames * 2` 安全余量，避免过度分配

**回调函数**：
```rust
// AudioUnit render_callback 处理逻辑
// 首次调用设置实时线程策略（只执行一次）
if ctx.thread_policy_set.compare_exchange(...).is_ok() {
    ctx.set_realtime_thread_policy();  // period 基于实际 buffer_frames
}

// 根据输出模式选择路径
match ctx.output_mode {
    Int32 => // 零拷贝：ring_buffer → 输出缓冲区直接读取
    Int24 => // 转换：ring_buffer → sample_buffer → 24-bit packed
    Float32 => // 转换 + dither：ring_buffer → sample_buffer → f32 + TPDF
}

// 支持 Interleaved 和 NonInterleaved 布局
// - Interleaved: LRLRLR... 所有样本在 mBuffers[0]
// - NonInterleaved: L 在 mBuffers[0], R 在 mBuffers[1]
```

### 3. 解码器 (`decode/decoder.rs`)

**职责**：音频文件解码，输出 i32 左对齐样本

**支持格式**：FLAC, WAV, AIFF, MP3（通过 symphonia）

**整数直通路径**：
- 对于 PCM 整数源（16/24/32-bit），直接转换到 i32，保持 bit-perfect
- 浮点源（f32/f64）通过浮点转换路径

**SIMD 优化**（ARM NEON，Apple Silicon）：
- 立体声 i16 → i32：`vmovl_s16` + `vshlq_n_s32` + `vst2q_s32` 交织存储
- 立体声 i24 → i32：`vld1q_s32` + `vshlq_n_s32` + `vst2q_s32` 交织存储
- 每次处理 4 帧（8 样本），`vst2q_s32` 一条指令完成 L/R 交织 + 存储
- 剩余帧标量处理

**样本格式（左对齐到 i32 高位）**：
```
16-bit: [S S S S S S S S S S S S S S S S 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0]
        |<------ 16-bit sample ------>|  |<-------- zero padding ------->|
        bit 31                      bit 16                              bit 0

24-bit: [S S S S S S S S S S S S S S S S S S S S S S S S 0 0 0 0 0 0 0 0]
        |<------------ 24-bit sample ------------->|    |<-- padding -->|
        bit 31                                    bit 8                bit 0

32-bit: [S S S S S S S S S S S S S S S S S S S S S S S S S S S S S S S S]
        |<---------------------- 32-bit sample ----------------------->|
```

### 4. 播放引擎 (`engine/mod.rs`)

**职责**：整合各模块，管理播放状态

**状态机**：
```
Stopped ──play()──→ Buffering ──prebuffer完成──→ Playing
    ↑                   │                          │
    │                   └──toggle_pause()──────────┤
    │                                              ↓
    └──────────────stop()──────────────────────Paused
```

**线程管理**：
- 解码线程：`QoS USER_INTERACTIVE` + `THREAD_TIME_CONSTRAINT_POLICY`（period=5ms, computation=2ms），回退 `nice -10`
- 解码线程亲和性标签 1（与 IO 线程同组，与 TUI 线程隔离）
- IO 线程：由 CoreAudio 管理，首次回调时设置 `THREAD_TIME_CONSTRAINT_POLICY`（period 基于设备 buffer_frames）

**暂停机制**：
- 使用 `Condvar` 实现零延迟唤醒
- 解码线程在暂停时等待，不消耗 CPU

**SRC 处理**：
- 当源采样率与设备采样率不匹配时，由 CoreAudio 内置 SRC 处理
- 解码线程直接写入源采样率数据到 ring buffer
- CoreAudio 自动转换到设备采样率

### 5. 播放统计 (`audio/stats.rs`)

**职责**：记录最基本的播放数据，IO 回调内开销最小化

**设计原则**：IO 回调内仅记录 `samples_played` 和 `underrun_count`，不做任何诊断性采样（interval timing、water level 等），确保信号路径上只有必要的计算。

**实现**：
- 两个 `AtomicU64` 字段，各自独占缓存行（`CacheLine<AtomicU64>`）
- IO 回调内仅需一次 `fetch_add(Relaxed)` 原子操作
- TUI 层在渲染循环中读取统计数据（与 IO 回调异步，无 cache line 竞争影响信号路径）

### 6. TUI 隔离 (`tui/` + `alloc.rs`)

**职责**：提供终端界面，同时确保对音频播放零干扰

**隔离措施**（按初始化顺序）：

| 措施 | 实现位置 | 说明 |
|------|----------|------|
| **堆内存隔离** | `alloc.rs`, `controller.rs` | macOS `malloc_create_zone()` 创建独立堆，TUI 线程的 malloc/free 不与音频线程竞争 |
| **线程优先级降低** | `controller.rs` | TUI 线程（主线程）设为系统最低优先级，永不抢占音频线程 CPU 时间 |
| **亲和性标签隔离** | `controller.rs` | TUI 标签 2，音频标签 1，调度器倾向将同标签线程分配到相近 CPU 核心 |
| **输入与渲染解耦** | `controller.rs` | 输入轮询 50ms，渲染间隔独立控制，避免渲染阻塞键盘响应 |

**全局内存分配器**（`alloc.rs`）：
```rust
#[global_allocator]
static GLOBAL: TuiIsolatedAllocator = TuiIsolatedAllocator;

// TUI 线程 → TUI malloc zone（独立内存池）
// 其他线程 → System allocator（默认行为）
// 释放/重分配 → macOS 自动识别指针所属 zone
```

消除的干扰源：
- **Cache pollution**: TUI 分配/释放不会驱逐音频线程的 L1/L2 cache line
- **Allocator contention**: TUI 的 malloc/free 不会与音频线程竞争全局堆锁
- **TLB 压力**: TUI 的页面访问模式不会影响音频线程的 TLB 命中率

### 7. 时间工具 (`audio/timing.rs`)

**职责**：Mach 时间相关转换

**功能**：
- Mach ticks 到纳秒转换（全局缓存 timebase info）
- 跨平台时间获取（macOS 使用 `mach_absolute_time`）
- Intel (1/1 timebase) 和 Apple Silicon (125/3 timebase) 自动适配

---

## 性能优化清单

### 时序稳定性优化

| 优化项 | 实现位置 | 说明 |
|--------|----------|------|
| **HALOutput 直接硬件访问** | `output.rs` | 绕过系统混音器，直接访问设备 |
| Lock-free SPSC Ring Buffer | `ring_buffer.rs` | 生产者/消费者完全无锁，wait-free |
| CacheLine 对齐 (128B) | `ring_buffer.rs` | `#[repr(C, align(128))]` 适配 Apple Silicon P-core 缓存行 |
| mlock 内存锁定 | `ring_buffer.rs`, `output.rs` | 防止 page fault 导致时序抖动 |
| IO 线程实时调度 | `output.rs` | `THREAD_TIME_CONSTRAINT_POLICY`（period 基于实际 buffer_frames） |
| 解码线程实时调度 | `engine/mod.rs` | `QoS USER_INTERACTIVE` + `THREAD_TIME_CONSTRAINT_POLICY` |
| TUI 堆内存隔离 | `alloc.rs` | macOS malloc zone，消除 cache pollution 和 allocator contention |
| TUI 线程优先级降低 | `controller.rs` | 最低优先级，永不抢占音频线程 |
| TUI 亲和性标签隔离 | `controller.rs` | 标签 2 vs 音频标签 1，减少跨核调度干扰 |
| IO 回调最小化 | `stats.rs`, `output.rs` | 回调内仅 ring_buffer read + samples_played/underrun 原子计数 |
| Condvar 暂停机制 | `engine/mod.rs` | 零延迟唤醒，避免轮询 |
| Mach 时间缓存 | `timing.rs` | timebase info 全局缓存，只查询一次 |

### Bit-Perfect 信号路径优化

| 优化项 | 实现位置 | 说明 |
|--------|----------|------|
| **HALOutput 直接访问** | `output.rs` | 绕过系统混音器，直接访问硬件 |
| 物理格式输出 | `output.rs` | 直接设置 `kAudioStreamPropertyPhysicalFormat`，绕过系统格式转换 |
| 零拷贝输出 (Int32) | `output.rs` | ring buffer → 输出缓冲区直接读取 |
| 整数直通路径 | `decoder.rs` | PCM 整数源直接转 i32，无浮点中间表示 |
| i32 左对齐统一格式 | `format.rs` | 全程 i32 左对齐，避免多次转换 |
| Hog Mode | `output.rs` | 独占设备，防止系统混音器干扰（HALOutput 模式） |
| 采样率智能选择 | `output.rs` | 精确匹配 > 整数分频 > 最近值 |
| CoreAudio SRC | 系统内置 | 高质量采样率转换，由 CoreAudio 处理 |
| TPDF Dither | `output.rs` | Float32 输出时使用，realtime-safe 实现 |

### 计算优化

| 优化项 | 实现位置 | 说明 |
|--------|----------|------|
| ARM NEON SIMD + vst2q 交织 | `decoder.rs` | 立体声 i16/i24 → i32：SIMD 转换 + 一条指令交织存储 |
| 2^n 容量位运算 | `ring_buffer.rs` | `mask & pos` 代替取模 |
| 内存预分配 | 全局 | 所有缓冲区初始化时分配，回调中无 alloc |
| TPDF Dither | `output.rs` | xorshift32 PRNG，realtime-safe |
| 回调缓冲区精确分配 | `output.rs` | `buffer_frames * 2` 而非固定 8192，减少内存浪费 |

### 输出模式优先级

```
Physical Format (Int32) → ASBD Integer → Float32 + Dither
       最优                  次优           回退
    bit-perfect          bit-perfect      有量化
```

### 可选改进（边际收益）

| 优化项 | 预期收益 | 说明 |
|--------|----------|------|
| i32/多声道 SIMD | 低 | 当前只有立体声 i16/i24 有 NEON |
| Ring Buffer 批量写入 | 低 | memcpy 替代循环，需处理 wrap |
| Prefetch 指令 | 微小 | 现代 CPU 预取已很智能 |
| 静音检测跳过 dither | 微小 | Float32 回退路径下消除 ~-144dB 噪声 |

---

## 开发注意事项

### 实时回调中的禁忌

```rust
// ❌ 绝对禁止
fn render_callback(...) {
    let vec = Vec::new();           // 内存分配
    mutex.lock();                    // 加锁
    log::info!("...");              // 日志（可能阻塞）
    std::fs::read(...);             // 文件 I/O
    thread::sleep(...);             // 睡眠
    stats.sample_interval();        // 诊断采样（多余计算）
}

// ✅ 正确做法
fn render_callback(...) {
    ring_buffer.read(&mut output);     // wait-free 原子操作
    stats.add_samples_played(count);   // 单次 fetch_add(Relaxed)
    // 所有数据在回调外预分配
}
```

### 原子操作 Ordering 选择

```rust
// 生产者写入后需要让消费者可见
write_pos.store(new_pos, Ordering::Release);

// 消费者读取前需要看到最新写入
let write = write_pos.load(Ordering::Acquire);

// 统计计数，无需跨线程同步
samples_played.fetch_add(count, Ordering::Relaxed);
```

### 添加新功能的检查清单

1. **是否在 IO 回调路径上？**
   - 是 → 必须 wait-free，无分配，无系统调用
   - 否 → 可以使用标准库功能

2. **是否需要跨线程共享？**
   - 是 → 使用原子类型或通过 ring buffer 传递
   - 否 → 普通变量即可

3. **是否影响音频路径？**
   - 是 → 需要性能测试，确保不增加 jitter
   - 否 → 正常开发

4. **是否涉及 TUI 线程的堆分配？**
   - 是 → 已通过全局分配器自动路由到 TUI zone，无需额外处理
   - 跨线程传递对象 → macOS `free()` 自动识别 zone，安全

---

## 扩展方向

### 已规划

- [ ] A/B 对比切换
- [ ] 播放列表支持
- [ ] DSD 支持

### 架构建议

**添加播放列表**：
```
Playlist Manager (主线程)
    │
    ├── 预加载下一曲 metadata
    ├── 无缝切换（gapless）：预解码 + 交叉淡入淡出
    └── 状态持久化
```

**添加 A/B 对比**：
```
Source A ──→ ┐
             ├─→ Switch ──→ Ring Buffer ──→ Output
Source B ──→ ┘
             ↑
         原子切换，延迟同步
```

---

## 测试

```bash
# 编译
cargo build --release

# 运行（需要音频文件）
./target/release/roger-player music.flac

# TUI 模式
./target/release/roger-player tui music_dir/

# 运行测试
cargo test

# 运行带设备的测试（需要音频设备）
cargo test -- --ignored
```

---

## 依赖

| Crate | 用途 |
|-------|------|
| symphonia | 音频解码（FLAC/WAV/AIFF/MP3） |
| clap | 命令行参数解析 |
| log + env_logger | 非阻塞日志 |
| libc | 系统调用（mlock, pthread, malloc zone） |
| crossbeam-utils | 并发工具 |
| ratatui + crossterm | 终端 UI |
| chrono | 时间格式化 |
| thiserror + anyhow | 错误处理 |
| rand | 随机数（播放列表 shuffle） |
| ctrlc | 信号处理 |
| coreaudio-sys | macOS Core Audio 绑定 |
| core-foundation | macOS Core Foundation 绑定 |

---

## 联系

如有问题，请参考：
- `src/` 各模块注释 - 实现细节
- 本文档 - 架构和开发指南
