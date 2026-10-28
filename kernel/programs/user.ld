/* A static user program for the Moruna microkernel: one segment per permission, each
   on its own pages, as the kernel's loader requires (SDD 20 f.2). */
ENTRY(_start)

PHDRS
{
    text PT_LOAD FLAGS(5);
    rodata PT_LOAD FLAGS(4);
    data PT_LOAD FLAGS(6);
}

SECTIONS
{
    . = 0x400000;
    .text : { *(.text .text.*) } :text

    . = ALIGN(4096);
    .rodata : { *(.rodata .rodata.*) } :rodata
    .eh_frame_hdr : { *(.eh_frame_hdr) } :rodata
    .eh_frame : { KEEP(*(.eh_frame)) } :rodata
    .gcc_except_table : { *(.gcc_except_table*) } :rodata

    . = ALIGN(4096);
    .data : { *(.data .data.*) } :data
    .got : { *(.got .got.*) } :data
    .bss : { *(.bss .bss.*) *(COMMON) } :data

    /DISCARD/ : { *(.comment) *(.note*) }
}
