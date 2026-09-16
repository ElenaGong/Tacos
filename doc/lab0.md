# Lab 1: Appetizer

<!-- --- -->

## Information

Name: 弓晓彤

Email: 2400013168@stu.pku.edu.cn

> Please cite any forms of information source that you have consulted during finishing your assignment, except the TacOS documentation, course slides, and course staff.

QEMU： https://github.com/qemu/qemu\
OpenSBI： https://github.com/riscv-software-src/opensbi/tree/master ，特别是 https://github.com/riscv-software-src/opensbi/blob/master/docs/domain_support.md 与 lab0 的问题 B6 相关。\
RISCV 文档： https://docs.riscv.org/reference/home/index.html ，从中查询了寄存器的名称和用途。\
Rust book： https://doc.rust-lang.org/book/  以及交互式学习网站 https://rust-book.cs.brown.edu/ 和 Rust Documentation： https://doc.rust-lang.org/stable/ 

> With any comments that may help TAs to evaluate your work better, please leave them here

## Booting Tacos

> A1: Put the screenshot of Tacos running example here.

<img src=assets/0-1-screenshot.png width=400>

<!-- ![alt text](assets/0-1-screenshot.png) -->

## Debugging

### First instruction

> B1: What is the first instruction that gets executed?

> B2: At which physical address is this instruction located?

进入debug之后立刻 `layout asm` 可以看到第一条指令是 `auipc t0, 0x0`，位于地址0x1000。此时系统尚未开启分页，该地址是 QEMU 启动代码所在的物理地址，因此物理地址就是 0x1000。

<!-- <img src=assets/0-2.png width=300> -->


### From ZSBL to SBI

> B3: Which address will the ZSBL jump to?

<!-- <img src=assets/0-3.png width=300> -->

在程序执行到 `jr t0` 时查看寄存器 `t0` 的值，为0x80000000

### SBI, kernel and argument passing

> B4: What's the value of the argument `hart_id` and `dtb`?

`hart_id`的值为0x0，`dtb`的值为0x82200000

> B5: What's the value of `Domain0 Next Address`, `Domain0 Next Arg1`, `Domain0 Next Mode` and `Boot HART ID` in OpenSBI's output?

```text
Domain0 Next Address      : 0x0000000080200000
Domain0 Next Arg1         : 0x0000000082200000
Domain0 Next Mode         : S-mode
Boot HART ID              : 0
```

> B6: What's the relationship between the four output values and the two arguments?

`Domain0 Next Mode: S-mode` 表明OpenSBI即将把控制权交给Tacos并以Supervisor模式运行，`Domain0 Next Address`标明Tacos的入口地址为0x0000000080200000。在这一转换发生时，`Boot HART ID`成为`hart_id`，`Domain0 Next Arg1`成为main函数的第二个参数`dtb`，所以它们相等。

### SBI interfaces

> B7: Inside `console_putchar`, Tacos uses `ecall` instruction to transfer control to SBI. What's the value of register `a6` and `a7` when executing that `ecall`?

给 `console_putchar` 打断点，发现程序停在 `ecall` 后面。在此时 `disassemble` 查看ecall指令的准确地址。重新调试，给此地址打断点，看到 `a6` 为0x0, `a7` 为0x1.

## Kernel Monitor

> C1: Put the screenshot of your kernel monitor running example here. (It should show how your kernel shell respond to `whoami`, `exit`, and `other input`.)

<img src=assets/0-5.png width=300>


> C2: Explain how you read and write to the console for the kernel monitor.

内核监视器通过 `sbi::console_getchar()` 每次读取一个字符。读取到有效字符后，先判断它是否是换行符；如果不是，就将它追加到预先初始化为空字符串的 `s` 中，读到换行符为止；\
然后将 `s` 去除首尾空格，与 `whoami` 、 `exit` 进行匹配，匹配成功则用 `kprint!` 输出相应内容（由于 `kprintln!` 还会输出时间，这里暂未使用）；对其他输入，输出"Invalid command"。