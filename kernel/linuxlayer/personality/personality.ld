/* The Linux personality's image (linuxlayer_launch): a static program linked at
   IMAGE_BASE, inside the personality's region, one segment per permission as the core's
   loader requires (SDD 20 f.2). The text segment opens with the table the launcher reads:
   the copy routines' start, end and recovery point. */
ENTRY(_start)

PHDRS
{
    text PT_LOAD FLAGS(5);
    rodata PT_LOAD FLAGS(4);
    data PT_LOAD FLAGS(6);
}

SECTIONS
{
    . = 0x500000000000;
    .linuxlayer_info : {
        QUAD(linuxlayer_user_start)
        QUAD(linuxlayer_user_end)
        QUAD(linuxlayer_user_fixup)
    } :text
    .text : { *(.text .text.*) } :text

    . = ALIGN(4096);
    .rodata : { *(.rodata .rodata.*) } :rodata
    .eh_frame_hdr : { *(.eh_frame_hdr) } :rodata
    .eh_frame : { KEEP(*(.eh_frame)) } :rodata
    .gcc_except_table : { *(.gcc_except_table*) } :rodata

    . = ALIGN(4096);
    .data : { *(.data .data.*) } :data
    /* The constructors `inventory` registers with; the start runs them. */
    .init_array : {
        __init_array_start = .;
        KEEP(*(SORT_BY_INIT_PRIORITY(.init_array.*) .init_array))
        __init_array_end = .;
    } :data
    .got : { *(.got .got.*) } :data
    .bss : { *(.bss .bss.*) *(COMMON) } :data

    /DISCARD/ : { *(.comment) *(.note*) }
}
