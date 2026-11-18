/* A static program with std for Moruna (item 5.0; kernel/tools/build-std.sh links it with this).
   The native programs' layout (kernel/sys/user.ld): one segment per permission, each on its own
   pages, as the kernel's loader requires (SDD 20 f.2), and two things std's runtime needs:
   - the ELF header and program headers mapped at the start of the first segment, so the runtime
     finds the TLS template's program header at `__ehdr_start` (moruna_sys::rt::tls);
   - the TLS template, `.tdata` and `.tbss`, in the data segment and its own PT_TLS, which each
     thread's block is made from (ELF TLS, x86_64 variant II; system design 5B).
   A function's `.note.moruna`, where it declares how many instances of it a job may run
   (system design 3E: moruna_function::processes!, MORUNA_PROCESSES), is kept in the read-only
   segment and its own PT_NOTE; every other note is dropped. */
ENTRY(_start)

PHDRS
{
    text PT_LOAD FILEHDR PHDRS FLAGS(5);
    rodata PT_LOAD FLAGS(4);
    data PT_LOAD FLAGS(6);
    tls PT_TLS FLAGS(4);
    note PT_NOTE FLAGS(4);
}

SECTIONS
{
    . = 0x400000 + SIZEOF_HEADERS;
    .text : { *(.text .text.*) } :text

    . = ALIGN(4096);
    .rodata : { *(.rodata .rodata.*) } :rodata
    .eh_frame_hdr : { *(.eh_frame_hdr) } :rodata
    .eh_frame : { KEEP(*(.eh_frame)) } :rodata
    .gcc_except_table : { *(.gcc_except_table*) } :rodata
    .note.moruna : { KEEP(*(.note.moruna)) } :rodata :note

    . = ALIGN(4096);
    .tdata : { *(.tdata .tdata.*) } :data :tls
    .tbss : { *(.tbss .tbss.*) *(.tcommon) } :data :tls
    /* The program's initializers and finalizers, which _start runs (item 5.2b): a C program's
       constructors, and the C library's own start. */
    .preinit_array : {
        PROVIDE_HIDDEN(__preinit_array_start = .);
        KEEP(*(.preinit_array))
        PROVIDE_HIDDEN(__preinit_array_end = .);
    } :data
    .init_array : {
        PROVIDE_HIDDEN(__init_array_start = .);
        KEEP(*(SORT_BY_INIT_PRIORITY(.init_array.*) SORT_BY_INIT_PRIORITY(.ctors.*)))
        KEEP(*(.init_array .ctors))
        PROVIDE_HIDDEN(__init_array_end = .);
    } :data
    .fini_array : {
        PROVIDE_HIDDEN(__fini_array_start = .);
        KEEP(*(SORT_BY_INIT_PRIORITY(.fini_array.*) SORT_BY_INIT_PRIORITY(.dtors.*)))
        KEEP(*(.fini_array .dtors))
        PROVIDE_HIDDEN(__fini_array_end = .);
    } :data
    .data : { *(.data .data.*) } :data
    .got : { *(.got .got.*) } :data
    .bss : { *(.bss .bss.*) *(COMMON) } :data

    /DISCARD/ : { *(.comment) *(.note*) }
}
