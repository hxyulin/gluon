ENTRY(_start)

PHDRS
{
    text   PT_LOAD FLAGS(5);  /* r-x */
    rodata PT_LOAD FLAGS(4);  /* r-- */
    data   PT_LOAD FLAGS(6);  /* rw- */
    note   PT_NOTE FLAGS(4);  /* r-- */
}

SECTIONS
{
    . = 0x100000;
    .text       : { *(.text*) }              :text

    /* PVH ELF note: tells QEMU >= 7.2 how to direct-boot this kernel.
       Generated here (not in Rust asm) to avoid R_X86_64_32 relocation
       errors in PIE mode. ABSOLUTE() emits a fixed value with no
       relocation record.

       Layout: namesz(4) descsz(4) type(4) name("Xen\0") desc(entry_addr)
       Type 18 = XEN_ELFNOTE_PHYS32_ENTRY.
       x86-specific; other architectures use different boot protocols. */
    .note.Xen   : {
        LONG(3);                                         /* namesz */
        LONG(4);                                         /* descsz */
        LONG(18);                                        /* type   */
        BYTE(0x58); BYTE(0x65); BYTE(0x6e); BYTE(0x00); /* "Xen\0" */
        LONG(ABSOLUTE(_start));                          /* entry  */
    } :note

    .rodata     : { *(.rodata*) }             :rodata
    .data       : { *(.data*) }               :data
    .bss        : { *(.bss*) }                :data
}
