/* RAM only. Clear of what the bootloader still uses while it loads and
 * jumps here: its loader code (0x40078000..0x40080000), its other code
 * (0x40080400..0x40090000), its data (0x3FFF0000..) and the ROM stack. */
MEMORY
{
  iram : ORIGIN = 0x40090000, LENGTH = 0x10000
  dram : ORIGIN = 0x3FFB0000, LENGTH = 0x20000
}
ENTRY(_start)
SECTIONS
{
  /* L32R reads literals at lower addresses than the code: literals first. */
  .text : ALIGN(4)
  {
    *(.literal .literal.*)
    *(.text .text.*)
  } > iram
  .rodata : ALIGN(4) { *(.rodata .rodata.*) } > dram
  .data : ALIGN(4) { *(.data .data.*) } > dram
  .bss (NOLOAD) : ALIGN(4)
  {
    _bss_start = .;
    *(.bss .bss.* COMMON)
    . = ALIGN(4);
    _bss_end = .;
  } > dram
  /DISCARD/ : { *(.xtensa.info) *(.comment) *(.note .note.*) *(.eh_frame*) }
}

/* ROM addresses and IDF's patched flash routines come from esp-rom-sys. */
