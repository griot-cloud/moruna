#ifndef _ELF_H
#define _ELF_H

#include <stdint.h>
#define ELFMAG "\x7f" "ELF"

#define ELF32_ST_BIND(val)      (((unsigned char) (val)) >> 4)
#define ELF32_ST_TYPE(val)      ((val) & 0xf)
#define ELF32_ST_INFO(bind, type)   (((bind) << 4) + ((type) & 0xf))

#define ELF64_ST_BIND(val)      ELF32_ST_BIND (val)
#define ELF64_ST_TYPE(val)      ELF32_ST_TYPE (val)
#define ELF64_ST_INFO(bind, type)   ELF32_ST_INFO ((bind), (type))


#define ELF32_ST_VISIBILITY(o)  ((o) & 0x03)
#define ELF64_ST_VISIBILITY(o)  ELF32_ST_VISIBILITY (o)

#define ELF32_R_SYM(val)        ((val) >> 8)
#define ELF32_R_TYPE(val)       ((val) & 0xff)
#define ELF32_R_INFO(sym, type)     (((sym) << 8) + ((type) & 0xff))

#define ELF64_R_SYM(i)          ((i) >> 32)
#define ELF64_R_TYPE(i)         ((i) & 0xffffffff)
#define ELF64_R_INFO(sym,type)      ((((Elf64_Xword) (sym)) << 32) + (type))

#define DT_VALRNGHI 0x6ffffdff
#define DT_VALTAGIDX(tag)   (DT_VALRNGHI - (tag))

#define DT_ADDRRNGHI    0x6ffffeff
#define DT_ADDRTAGIDX(tag)  (DT_ADDRRNGHI - (tag))

#define DT_VERNEEDNUM   0x6fffffff
#define DT_VERSIONTAGIDX(tag)   (DT_VERNEEDNUM - (tag))

#define ELF32_M_SYM(info)   ((info) >> 8)
#define ELF32_M_SIZE(info)  ((unsigned char) (info))
#define ELF32_M_INFO(sym, size) (((sym) << 8) + (unsigned char) (size))

#define ELF64_M_SYM(info)   ELF32_M_SYM (info)
#define ELF64_M_SIZE(info)  ELF32_M_SIZE (info)
#define ELF64_M_INFO(sym, size) ELF32_M_INFO (sym, size)


#define EI_NIDENT 16

#define EI_MAG0 0

#define ELFMAG0 127

#define EI_MAG1 1

#define EI_MAG2 2

#define EI_MAG3 3

#define SELFMAG 4

#define EI_CLASS 4

#define ELFCLASSNONE 0

#define ELFCLASS32 1

#define ELFCLASS64 2

#define ELFCLASSNUM 3

#define EI_DATA 5

#define ELFDATANONE 0

#define ELFDATA2LSB 1

#define ELFDATA2MSB 2

#define ELFDATANUM 3

#define EI_VERSION 6

#define EI_OSABI 7

#define ELFOSABI_NONE 0

#define ELFOSABI_SYSV 0

#define ELFOSABI_HPUX 1

#define ELFOSABI_NETBSD 2

#define ELFOSABI_LINUX 3

#define ELFOSABI_GNU 3

#define ELFOSABI_SOLARIS 6

#define ELFOSABI_AIX 7

#define ELFOSABI_IRIX 8

#define ELFOSABI_FREEBSD 9

#define ELFOSABI_TRU64 10

#define ELFOSABI_MODESTO 11

#define ELFOSABI_OPENBSD 12

#define ELFOSABI_ARM 97

#define ELFOSABI_STANDALONE 255

#define EI_ABIVERSION 8

#define EI_PAD 9

#define ET_NONE 0

#define ET_REL 1

#define ET_EXEC 2

#define ET_DYN 3

#define ET_CORE 4

#define ET_NUM 5

#define ET_LOOS 65024

#define ET_HIOS 65279

#define ET_LOPROC 65280

#define ET_HIPROC 65535

#define EM_NONE 0

#define EM_M32 1

#define EM_SPARC 2

#define EM_386 3

#define EM_68K 4

#define EM_88K 5

#define EM_860 7

#define EM_MIPS 8

#define EM_S370 9

#define EM_MIPS_RS3_LE 10

#define EM_PARISC 15

#define EM_VPP500 17

#define EM_SPARC32PLUS 18

#define EM_960 19

#define EM_PPC 20

#define EM_PPC64 21

#define EM_S390 22

#define EM_V800 36

#define EM_FR20 37

#define EM_RH32 38

#define EM_RCE 39

#define EM_ARM 40

#define EM_FAKE_ALPHA 41

#define EM_SH 42

#define EM_SPARCV9 43

#define EM_TRICORE 44

#define EM_ARC 45

#define EM_H8_300 46

#define EM_H8_300H 47

#define EM_H8S 48

#define EM_H8_500 49

#define EM_IA_64 50

#define EM_MIPS_X 51

#define EM_COLDFIRE 52

#define EM_68HC12 53

#define EM_MMA 54

#define EM_PCP 55

#define EM_NCPU 56

#define EM_NDR1 57

#define EM_STARCORE 58

#define EM_ME16 59

#define EM_ST100 60

#define EM_TINYJ 61

#define EM_X86_64 62

#define EM_PDSP 63

#define EM_FX66 66

#define EM_ST9PLUS 67

#define EM_ST7 68

#define EM_68HC16 69

#define EM_68HC11 70

#define EM_68HC08 71

#define EM_68HC05 72

#define EM_SVX 73

#define EM_ST19 74

#define EM_VAX 75

#define EM_CRIS 76

#define EM_JAVELIN 77

#define EM_FIREPATH 78

#define EM_ZSP 79

#define EM_MMIX 80

#define EM_HUANY 81

#define EM_PRISM 82

#define EM_AVR 83

#define EM_FR30 84

#define EM_D10V 85

#define EM_D30V 86

#define EM_V850 87

#define EM_M32R 88

#define EM_MN10300 89

#define EM_MN10200 90

#define EM_PJ 91

#define EM_OR1K 92

#define EM_ARC_A5 93

#define EM_XTENSA 94

#define EM_AARCH64 183

#define EM_TILEPRO 188

#define EM_MICROBLAZE 189

#define EM_TILEGX 191

#define EM_NUM 192

#define EM_ALPHA 36902

#define EV_NONE 0

#define EV_CURRENT 1

#define EV_NUM 2

#define SHN_UNDEF 0

#define SHN_LORESERVE 65280

#define SHN_LOPROC 65280

#define SHN_BEFORE 65280

#define SHN_AFTER 65281

#define SHN_HIPROC 65311

#define SHN_LOOS 65312

#define SHN_HIOS 65343

#define SHN_ABS 65521

#define SHN_COMMON 65522

#define SHN_XINDEX 65535

#define SHN_HIRESERVE 65535

#define SHT_NULL 0

#define SHT_PROGBITS 1

#define SHT_SYMTAB 2

#define SHT_STRTAB 3

#define SHT_RELA 4

#define SHT_HASH 5

#define SHT_DYNAMIC 6

#define SHT_NOTE 7

#define SHT_NOBITS 8

#define SHT_REL 9

#define SHT_SHLIB 10

#define SHT_DYNSYM 11

#define SHT_INIT_ARRAY 14

#define SHT_FINI_ARRAY 15

#define SHT_PREINIT_ARRAY 16

#define SHT_GROUP 17

#define SHT_SYMTAB_SHNDX 18

#define SHT_NUM 19

#define SHT_LOOS 1610612736

#define SHT_GNU_ATTRIBUTES 1879048181

#define SHT_GNU_HASH 1879048182

#define SHT_GNU_LIBLIST 1879048183

#define SHT_CHECKSUM 1879048184

#define SHT_LOSUNW 1879048186

#define SHT_SUNW_move 1879048186

#define SHT_SUNW_COMDAT 1879048187

#define SHT_SUNW_syminfo 1879048188

#define SHT_GNU_verdef 1879048189

#define SHT_GNU_verneed 1879048190

#define SHT_GNU_versym 1879048191

#define SHT_HISUNW 1879048191

#define SHT_HIOS 1879048191

#define SHT_LOPROC 1879048192

#define SHT_HIPROC 2147483647

#define SHT_LOUSER 2147483648

#define SHT_HIUSER 2415919103

#define SHF_WRITE (1 << 0)

#define SHF_ALLOC (1 << 1)

#define SHF_EXECINSTR (1 << 2)

#define SHF_MERGE (1 << 4)

#define SHF_STRINGS (1 << 5)

#define SHF_INFO_LINK (1 << 6)

#define SHF_LINK_ORDER (1 << 7)

#define SHF_OS_NONCONFORMING (1 << 8)

#define SHF_GROUP (1 << 9)

#define SHF_TLS (1 << 10)

#define SHF_MASKOS 267386880

#define SHF_MASKPROC 4026531840

#define SHF_ORDERED (1 << 30)

#define SHF_EXCLUDE (1 << 31)

#define GRP_COMDAT 1

#define SYMINFO_BT_SELF 65535

#define SYMINFO_BT_PARENT 65534

#define SYMINFO_BT_LOWRESERVE 65280

#define SYMINFO_FLG_DIRECT 1

#define SYMINFO_FLG_PASSTHRU 2

#define SYMINFO_FLG_COPY 4

#define SYMINFO_FLG_LAZYLOAD 8

#define SYMINFO_NONE 0

#define SYMINFO_CURRENT 1

#define SYMINFO_NUM 2

#define STB_LOCAL 0

#define STB_GLOBAL 1

#define STB_WEAK 2

#define STB_NUM 3

#define STB_LOOS 10

#define STB_GNU_UNIQUE 10

#define STB_HIOS 12

#define STB_LOPROC 13

#define STB_HIPROC 15

#define STT_NOTYPE 0

#define STT_OBJECT 1

#define STT_FUNC 2

#define STT_SECTION 3

#define STT_FILE 4

#define STT_COMMON 5

#define STT_TLS 6

#define STT_NUM 7

#define STT_LOOS 10

#define STT_GNU_IFUNC 10

#define STT_HIOS 12

#define STT_LOPROC 13

#define STT_HIPROC 15

#define STN_UNDEF 0

#define STV_DEFAULT 0

#define STV_INTERNAL 1

#define STV_HIDDEN 2

#define STV_PROTECTED 3

#define PT_NULL 0

#define PT_LOAD 1

#define PT_DYNAMIC 2

#define PT_INTERP 3

#define PT_NOTE 4

#define PT_SHLIB 5

#define PT_PHDR 6

#define PT_TLS 7

#define PT_NUM 8

#define PT_LOOS 1610612736

#define PT_GNU_EH_FRAME 1685382480

#define PT_GNU_STACK 1685382481

#define PT_GNU_RELRO 1685382482

#define PT_LOSUNW 1879048186

#define PT_SUNWBSS 1879048186

#define PT_SUNWSTACK 1879048187

#define PT_HISUNW 1879048191

#define PT_HIOS 1879048191

#define PT_LOPROC 1879048192

#define PT_HIPROC 2147483647

#define PN_XNUM 65535

#define PF_X (1 << 0)

#define PF_W (1 << 1)

#define PF_R (1 << 2)

#define PF_MASKOS 267386880

#define PF_MASKPROC 4026531840

#define NT_PRSTATUS 1

#define NT_FPREGSET 2

#define NT_PRPSINFO 3

#define NT_PRXREG 4

#define NT_TASKSTRUCT 4

#define NT_PLATFORM 5

#define NT_AUXV 6

#define NT_GWINDOWS 7

#define NT_ASRS 8

#define NT_PSTATUS 10

#define NT_PSINFO 13

#define NT_PRCRED 14

#define NT_UTSNAME 15

#define NT_LWPSTATUS 16

#define NT_LWPSINFO 17

#define NT_PRFPXREG 20

#define NT_SIGINFO 1397311305

#define NT_FILE 1179208773

#define NT_PRXFPREG 1189489535

#define NT_PPC_VMX 256

#define NT_PPC_SPE 257

#define NT_PPC_VSX 258

#define NT_386_TLS 512

#define NT_386_IOPERM 513

#define NT_X86_XSTATE 514

#define NT_S390_HIGH_GPRS 768

#define NT_S390_TIMER 769

#define NT_S390_TODCMP 770

#define NT_S390_TODPREG 771

#define NT_S390_CTRS 772

#define NT_S390_PREFIX 773

#define NT_S390_LAST_BREAK 774

#define NT_S390_SYSTEM_CALL 775

#define NT_S390_TDB 776

#define NT_ARM_VFP 1024

#define NT_ARM_TLS 1025

#define NT_ARM_HW_BREAK 1026

#define NT_ARM_HW_WATCH 1027

#define NT_METAG_CBUF 1280

#define NT_METAG_RPIPE 1281

#define NT_METAG_TLS 1282

#define NT_VERSION 1

#define DT_NULL 0

#define DT_NEEDED 1

#define DT_PLTRELSZ 2

#define DT_PLTGOT 3

#define DT_HASH 4

#define DT_STRTAB 5

#define DT_SYMTAB 6

#define DT_RELA 7

#define DT_RELASZ 8

#define DT_RELAENT 9

#define DT_STRSZ 10

#define DT_SYMENT 11

#define DT_INIT 12

#define DT_FINI 13

#define DT_SONAME 14

#define DT_RPATH 15

#define DT_SYMBOLIC 16

#define DT_REL 17

#define DT_RELSZ 18

#define DT_RELENT 19

#define DT_PLTREL 20

#define DT_DEBUG 21

#define DT_TEXTREL 22

#define DT_JMPREL 23

#define DT_BIND_NOW 24

#define DT_INIT_ARRAY 25

#define DT_FINI_ARRAY 26

#define DT_INIT_ARRAYSZ 27

#define DT_FINI_ARRAYSZ 28

#define DT_RUNPATH 29

#define DT_FLAGS 30

#define DT_ENCODING 32

#define DT_PREINIT_ARRAY 32

#define DT_PREINIT_ARRAYSZ 33

#define DT_NUM 34

#define DT_LOOS 1610612749

#define DT_HIOS 1879044096

#define DT_LOPROC 1879048192

#define DT_HIPROC 2147483647

#define DT_VALRNGLO 1879047424

#define DT_GNU_PRELINKED 1879047669

#define DT_GNU_CONFLICTSZ 1879047670

#define DT_GNU_LIBLISTSZ 1879047671

#define DT_CHECKSUM 1879047672

#define DT_PLTPADSZ 1879047673

#define DT_MOVEENT 1879047674

#define DT_MOVESZ 1879047675

#define DT_FEATURE_1 1879047676

#define DT_POSFLAG_1 1879047677

#define DT_SYMINSZ 1879047678

#define DT_SYMINENT 1879047679

#define DT_VALNUM 12

#define DT_ADDRRNGLO 1879047680

#define DT_GNU_HASH 1879047925

#define DT_TLSDESC_PLT 1879047926

#define DT_TLSDESC_GOT 1879047927

#define DT_GNU_CONFLICT 1879047928

#define DT_GNU_LIBLIST 1879047929

#define DT_CONFIG 1879047930

#define DT_DEPAUDIT 1879047931

#define DT_AUDIT 1879047932

#define DT_PLTPAD 1879047933

#define DT_MOVETAB 1879047934

#define DT_SYMINFO 1879047935

#define DT_ADDRNUM 11

#define DT_VERSYM 1879048176

#define DT_RELACOUNT 1879048185

#define DT_RELCOUNT 1879048186

#define DT_FLAGS_1 1879048187

#define DT_VERDEF 1879048188

#define DT_VERDEFNUM 1879048189

#define DT_VERNEED 1879048190

#define DT_VERSIONTAGNUM 16

#define DT_AUXILIARY 2147483645

#define DT_FILTER 2147483647

#define DT_EXTRANUM 3

#define DF_ORIGIN 1

#define DF_SYMBOLIC 2

#define DF_TEXTREL 4

#define DF_BIND_NOW 8

#define DF_STATIC_TLS 16

#define DF_1_NOW 1

#define DF_1_GLOBAL 2

#define DF_1_GROUP 4

#define DF_1_NODELETE 8

#define DF_1_LOADFLTR 16

#define DF_1_INITFIRST 32

#define DF_1_NOOPEN 64

#define DF_1_ORIGIN 128

#define DF_1_DIRECT 256

#define DF_1_TRANS 512

#define DF_1_INTERPOSE 1024

#define DF_1_NODEFLIB 2048

#define DF_1_NODUMP 4096

#define DF_1_CONFALT 8192

#define DF_1_ENDFILTEE 16384

#define DF_1_DISPRELDNE 32768

#define DF_1_DISPRELPND 65536

#define DF_1_NODIRECT 131072

#define DF_1_IGNMULDEF 262144

#define DF_1_NOKSYMS 524288

#define DF_1_NOHDR 1048576

#define DF_1_EDITED 2097152

#define DF_1_NORELOC 4194304

#define DF_1_SYMINTPOSE 8388608

#define DF_1_GLOBAUDIT 16777216

#define DF_1_SINGLETON 33554432

#define DTF_1_PARINIT 1

#define DTF_1_CONFEXP 2

#define DF_P1_LAZYLOAD 1

#define DF_P1_GROUPPERM 2

#define VER_DEF_NONE 0

#define VER_DEF_CURRENT 1

#define VER_DEF_NUM 2

#define VER_FLG_BASE 1

#define VER_FLG_WEAK 2

#define VER_NDX_LOCAL 0

#define VER_NDX_GLOBAL 1

#define VER_NDX_LORESERVE 65280

#define VER_NDX_ELIMINATE 65281

#define VER_NEED_NONE 0

#define VER_NEED_CURRENT 1

#define VER_NEED_NUM 2

#define AT_NULL 0

#define AT_IGNORE 1

#define AT_EXECFD 2

#define AT_PHDR 3

#define AT_PHENT 4

#define AT_PHNUM 5

#define AT_PAGESZ 6

#define AT_BASE 7

#define AT_FLAGS 8

#define AT_ENTRY 9

#define AT_NOTELF 10

#define AT_UID 11

#define AT_EUID 12

#define AT_GID 13

#define AT_EGID 14

#define AT_CLKTCK 17

#define AT_PLATFORM 15

#define AT_HWCAP 16

#define AT_FPUCW 18

#define AT_DCACHEBSIZE 19

#define AT_ICACHEBSIZE 20

#define AT_UCACHEBSIZE 21

#define AT_IGNOREPPC 22

#define AT_SECURE 23

#define AT_BASE_PLATFORM 24

#define AT_RANDOM 25

#define AT_HWCAP2 26

#define AT_EXECFN 31

#define AT_SYSINFO 32

#define AT_SYSINFO_EHDR 33

#define AT_L1I_CACHESHAPE 34

#define AT_L1D_CACHESHAPE 35

#define AT_L2_CACHESHAPE 36

#define AT_L3_CACHESHAPE 37

#define ELF_NOTE_PAGESIZE_HINT 1

#define NT_GNU_ABI_TAG 1

#define ELF_NOTE_ABI NT_GNU_ABI_TAG

#define ELF_NOTE_OS_LINUX 0

#define ELF_NOTE_OS_GNU 1

#define ELF_NOTE_OS_SOLARIS2 2

#define ELF_NOTE_OS_FREEBSD 3

#define NT_GNU_BUILD_ID 3

#define NT_GNU_GOLD_VERSION 4

#define R_AARCH64_NONE 0

#define R_AARCH64_ABS64 257

#define R_AARCH64_ABS32 258

#define R_AARCH64_ABS16 259

#define R_AARCH64_PREL64 260

#define R_AARCH64_PREL32 261

#define R_AARCH64_PREL16 262

#define R_AARCH64_MOVW_UABS_G0 263

#define R_AARCH64_MOVW_UABS_G0_NC 264

#define R_AARCH64_MOVW_UABS_G1 265

#define R_AARCH64_MOVW_UABS_G1_NC 266

#define R_AARCH64_MOVW_UABS_G2 267

#define R_AARCH64_MOVW_UABS_G2_NC 268

#define R_AARCH64_MOVW_UABS_G3 269

#define R_AARCH64_MOVW_SABS_G0 270

#define R_AARCH64_MOVW_SABS_G1 271

#define R_AARCH64_MOVW_SABS_G2 272

#define R_AARCH64_LD_PREL_LO19 273

#define R_AARCH64_ADR_PREL_LO21 274

#define R_AARCH64_ADR_PREL_PG_HI21 275

#define R_AARCH64_ADR_PREL_PG_HI21_NC 276

#define R_AARCH64_ADD_ABS_LO12_NC 277

#define R_AARCH64_LDST8_ABS_LO12_NC 278

#define R_AARCH64_TSTBR14 279

#define R_AARCH64_CONDBR19 280

#define R_AARCH64_JUMP26 282

#define R_AARCH64_CALL26 283

#define R_AARCH64_LDST16_ABS_LO12_NC 284

#define R_AARCH64_LDST32_ABS_LO12_NC 285

#define R_AARCH64_LDST64_ABS_LO12_NC 286

#define R_AARCH64_MOVW_PREL_G0 287

#define R_AARCH64_MOVW_PREL_G0_NC 288

#define R_AARCH64_MOVW_PREL_G1 289

#define R_AARCH64_MOVW_PREL_G1_NC 290

#define R_AARCH64_MOVW_PREL_G2 291

#define R_AARCH64_MOVW_PREL_G2_NC 292

#define R_AARCH64_MOVW_PREL_G3 293

#define R_AARCH64_LDST128_ABS_LO12_NC 299

#define R_AARCH64_MOVW_GOTOFF_G0 300

#define R_AARCH64_MOVW_GOTOFF_G0_NC 301

#define R_AARCH64_MOVW_GOTOFF_G1 302

#define R_AARCH64_MOVW_GOTOFF_G1_NC 303

#define R_AARCH64_MOVW_GOTOFF_G2 304

#define R_AARCH64_MOVW_GOTOFF_G2_NC 305

#define R_AARCH64_MOVW_GOTOFF_G3 306

#define R_AARCH64_GOTREL64 307

#define R_AARCH64_GOTREL32 308

#define R_AARCH64_GOT_LD_PREL19 309

#define R_AARCH64_LD64_GOTOFF_LO15 310

#define R_AARCH64_ADR_GOT_PAGE 311

#define R_AARCH64_LD64_GOT_LO12_NC 312

#define R_AARCH64_LD64_GOTPAGE_LO15 313

#define R_AARCH64_TLSGD_ADR_PREL21 512

#define R_AARCH64_TLSGD_ADR_PAGE21 513

#define R_AARCH64_TLSGD_ADD_LO12_NC 514

#define R_AARCH64_TLSGD_MOVW_G1 515

#define R_AARCH64_TLSGD_MOVW_G0_NC 516

#define R_AARCH64_TLSLD_ADR_PREL21 517

#define R_AARCH64_TLSLD_ADR_PAGE21 518

#define R_AARCH64_TLSLD_ADD_LO12_NC 519

#define R_AARCH64_TLSLD_MOVW_G1 520

#define R_AARCH64_TLSLD_MOVW_G0_NC 521

#define R_AARCH64_TLSLD_LD_PREL19 522

#define R_AARCH64_TLSLD_MOVW_DTPREL_G2 523

#define R_AARCH64_TLSLD_MOVW_DTPREL_G1 524

#define R_AARCH64_TLSLD_MOVW_DTPREL_G1_NC 525

#define R_AARCH64_TLSLD_MOVW_DTPREL_G0 526

#define R_AARCH64_TLSLD_MOVW_DTPREL_G0_NC 527

#define R_AARCH64_TLSLD_ADD_DTPREL_HI12 528

#define R_AARCH64_TLSLD_ADD_DTPREL_LO12 529

#define R_AARCH64_TLSLD_ADD_DTPREL_LO12_NC 530

#define R_AARCH64_TLSLD_LDST8_DTPREL_LO12 531

#define R_AARCH64_TLSLD_LDST8_DTPREL_LO12_NC 532

#define R_AARCH64_TLSLD_LDST16_DTPREL_LO12 533

#define R_AARCH64_TLSLD_LDST16_DTPREL_LO12_NC 534

#define R_AARCH64_TLSLD_LDST32_DTPREL_LO12 535

#define R_AARCH64_TLSLD_LDST32_DTPREL_LO12_NC 536

#define R_AARCH64_TLSLD_LDST64_DTPREL_LO12 537

#define R_AARCH64_TLSLD_LDST64_DTPREL_LO12_NC 538

#define R_AARCH64_TLSIE_MOVW_GOTTPREL_G1 539

#define R_AARCH64_TLSIE_MOVW_GOTTPREL_G0_NC 540

#define R_AARCH64_TLSIE_ADR_GOTTPREL_PAGE21 541

#define R_AARCH64_TLSIE_LD64_GOTTPREL_LO12_NC 542

#define R_AARCH64_TLSIE_LD_GOTTPREL_PREL19 543

#define R_AARCH64_TLSLE_MOVW_TPREL_G2 544

#define R_AARCH64_TLSLE_MOVW_TPREL_G1 545

#define R_AARCH64_TLSLE_MOVW_TPREL_G1_NC 546

#define R_AARCH64_TLSLE_MOVW_TPREL_G0 547

#define R_AARCH64_TLSLE_MOVW_TPREL_G0_NC 548

#define R_AARCH64_TLSLE_ADD_TPREL_HI12 549

#define R_AARCH64_TLSLE_ADD_TPREL_LO12 550

#define R_AARCH64_TLSLE_ADD_TPREL_LO12_NC 551

#define R_AARCH64_TLSLE_LDST8_TPREL_LO12 552

#define R_AARCH64_TLSLE_LDST8_TPREL_LO12_NC 553

#define R_AARCH64_TLSLE_LDST16_TPREL_LO12 554

#define R_AARCH64_TLSLE_LDST16_TPREL_LO12_NC 555

#define R_AARCH64_TLSLE_LDST32_TPREL_LO12 556

#define R_AARCH64_TLSLE_LDST32_TPREL_LO12_NC 557

#define R_AARCH64_TLSLE_LDST64_TPREL_LO12 558

#define R_AARCH64_TLSLE_LDST64_TPREL_LO12_NC 559

#define R_AARCH64_TLSDESC_LD_PREL19 560

#define R_AARCH64_TLSDESC_ADR_PREL21 561

#define R_AARCH64_TLSDESC_ADR_PAGE21 562

#define R_AARCH64_TLSDESC_LD64_LO12 563

#define R_AARCH64_TLSDESC_ADD_LO12 564

#define R_AARCH64_TLSDESC_OFF_G1 565

#define R_AARCH64_TLSDESC_OFF_G0_NC 566

#define R_AARCH64_TLSDESC_LDR 567

#define R_AARCH64_TLSDESC_ADD 568

#define R_AARCH64_TLSDESC_CALL 569

#define R_AARCH64_TLSLE_LDST128_TPREL_LO12 570

#define R_AARCH64_TLSLE_LDST128_TPREL_LO12_NC 571

#define R_AARCH64_TLSLD_LDST128_DTPREL_LO12 572

#define R_AARCH64_TLSLD_LDST128_DTPREL_LO12_NC 573

#define R_AARCH64_COPY 1024

#define R_AARCH64_GLOB_DAT 1025

#define R_AARCH64_JUMP_SLOT 1026

#define R_AARCH64_RELATIVE 1027

#define R_AARCH64_TLS_DTPMOD64 1028

#define R_AARCH64_TLS_DTPREL64 1029

#define R_AARCH64_TLS_TPREL64 1030

#define R_AARCH64_TLSDESC 1031

#define R_X86_64_NONE 0

#define R_X86_64_64 1

#define R_X86_64_PC32 2

#define R_X86_64_GOT32 3

#define R_X86_64_PLT32 4

#define R_X86_64_COPY 5

#define R_X86_64_GLOB_DAT 6

#define R_X86_64_JUMP_SLOT 7

#define R_X86_64_RELATIVE 8

#define R_X86_64_GOTPCREL 9

#define R_X86_64_32 10

#define R_X86_64_32S 11

#define R_X86_64_16 12

#define R_X86_64_PC16 13

#define R_X86_64_8 14

#define R_X86_64_PC8 15

#define R_X86_64_DTPMOD64 16

#define R_X86_64_DTPOFF64 17

#define R_X86_64_TPOFF64 18

#define R_X86_64_TLSGD 19

#define R_X86_64_TLSLD 20

#define R_X86_64_DTPOFF32 21

#define R_X86_64_GOTTPOFF 22

#define R_X86_64_TPOFF32 23

#define R_X86_64_PC64 24

#define R_X86_64_GOTOFF64 25

#define R_X86_64_GOTPC32 26

#define R_X86_64_GOT64 27

#define R_X86_64_GOTPCREL64 28

#define R_X86_64_GOTPC64 29

#define R_X86_64_GOTPLT64 30

#define R_X86_64_PLTOFF64 31

#define R_X86_64_SIZE32 32

#define R_X86_64_SIZE64 33

#define R_X86_64_GOTPC32_TLSDESC 34

#define R_X86_64_TLSDESC_CALL 35

#define R_X86_64_TLSDESC 36

#define R_X86_64_IRELATIVE 37

#define R_X86_64_RELATIVE64 38

#define R_X86_64_NUM 39

typedef uint16_t Elf32_Half;

typedef uint32_t Elf32_Word;

typedef uint32_t Elf32_Addr;

typedef uint32_t Elf32_Off;

/**
 * See <https://www.man7.org/linux/man-pages/man5/elf.5.html>.
 */
typedef struct Elf32_Ehdr {
  unsigned char e_ident[EI_NIDENT];
  Elf32_Half e_type;
  Elf32_Half e_machine;
  Elf32_Word e_version;
  Elf32_Addr e_entry;
  Elf32_Off e_phoff;
  Elf32_Off e_shoff;
  Elf32_Word e_flags;
  Elf32_Half e_ehsize;
  Elf32_Half e_phentsize;
  Elf32_Half e_phnum;
  Elf32_Half e_shentsize;
  Elf32_Half e_shnum;
  Elf32_Half e_shstrndx;
} Elf32_Ehdr;

typedef uint16_t Elf64_Half;

typedef uint32_t Elf64_Word;

typedef uint64_t Elf64_Addr;

typedef uint64_t Elf64_Off;

/**
 * See <https://www.man7.org/linux/man-pages/man5/elf.5.html>.
 */
typedef struct Elf64_Ehdr {
  unsigned char e_ident[EI_NIDENT];
  Elf64_Half e_type;
  Elf64_Half e_machine;
  Elf64_Word e_version;
  Elf64_Addr e_entry;
  Elf64_Off e_phoff;
  Elf64_Off e_shoff;
  Elf64_Word e_flags;
  Elf64_Half e_ehsize;
  Elf64_Half e_phentsize;
  Elf64_Half e_phnum;
  Elf64_Half e_shentsize;
  Elf64_Half e_shnum;
  Elf64_Half e_shstrndx;
} Elf64_Ehdr;

/**
 * See <https://www.man7.org/linux/man-pages/man5/elf.5.html>.
 */
typedef struct Elf32_Shdr {
  Elf32_Word sh_name;
  Elf32_Word sh_type;
  Elf32_Word sh_flags;
  Elf32_Addr sh_addr;
  Elf32_Off sh_offset;
  Elf32_Word sh_size;
  Elf32_Word sh_link;
  Elf32_Word sh_info;
  Elf32_Word sh_addralign;
  Elf32_Word sh_entsize;
} Elf32_Shdr;

typedef uint64_t Elf64_Xword;

/**
 * See <https://www.man7.org/linux/man-pages/man5/elf.5.html>.
 */
typedef struct Elf64_Shdr {
  Elf64_Word sh_name;
  Elf64_Word sh_type;
  Elf64_Xword sh_flags;
  Elf64_Addr sh_addr;
  Elf64_Off sh_offset;
  Elf64_Xword sh_size;
  Elf64_Word sh_link;
  Elf64_Word sh_info;
  Elf64_Xword sh_addralign;
  Elf64_Xword sh_entsize;
} Elf64_Shdr;

typedef uint16_t Elf32_Section;

/**
 * See <https://www.man7.org/linux/man-pages/man5/elf.5.html>.
 */
typedef struct Elf32_Sym {
  Elf32_Word st_name;
  Elf32_Addr st_value;
  Elf32_Word st_size;
  unsigned char st_info;
  unsigned char st_other;
  Elf32_Section st_shndx;
} Elf32_Sym;

typedef uint16_t Elf64_Section;

/**
 * See <https://www.man7.org/linux/man-pages/man5/elf.5.html>.
 */
typedef struct Elf64_Sym {
  Elf64_Word st_name;
  unsigned char st_info;
  unsigned char st_other;
  Elf64_Section st_shndx;
  Elf64_Addr st_value;
  Elf64_Xword st_size;
} Elf64_Sym;

typedef struct Elf32_Syminfo {
  Elf32_Half si_boundto;
  Elf32_Half si_flags;
} Elf32_Syminfo;

typedef struct Elf64_Syminfo {
  Elf64_Half si_boundto;
  Elf64_Half si_flags;
} Elf64_Syminfo;

/**
 * See <https://www.man7.org/linux/man-pages/man5/elf.5.html>.
 */
typedef struct Elf32_Rel {
  Elf32_Addr r_offset;
  Elf32_Word r_info;
} Elf32_Rel;

/**
 * See <https://www.man7.org/linux/man-pages/man5/elf.5.html>.
 */
typedef struct Elf64_Rel {
  Elf64_Addr r_offset;
  Elf64_Xword r_info;
} Elf64_Rel;

typedef int32_t Elf32_Sword;

/**
 * See <https://www.man7.org/linux/man-pages/man5/elf.5.html>.
 */
typedef struct Elf32_Rela {
  Elf32_Addr r_offset;
  Elf32_Word r_info;
  Elf32_Sword r_addend;
} Elf32_Rela;

typedef int64_t Elf64_Sxword;

/**
 * See <https://www.man7.org/linux/man-pages/man5/elf.5.html>.
 */
typedef struct Elf64_Rela {
  Elf64_Addr r_offset;
  Elf64_Xword r_info;
  Elf64_Sxword r_addend;
} Elf64_Rela;

/**
 * See <https://www.man7.org/linux/man-pages/man5/elf.5.html>.
 */
typedef struct Elf32_Phdr {
  Elf32_Word p_type;
  Elf32_Off p_offset;
  Elf32_Addr p_vaddr;
  Elf32_Addr p_paddr;
  Elf32_Word p_filesz;
  Elf32_Word p_memsz;
  Elf32_Word p_flags;
  Elf32_Word p_align;
} Elf32_Phdr;

/**
 * See <https://www.man7.org/linux/man-pages/man5/elf.5.html>.
 */
typedef struct Elf64_Phdr {
  Elf64_Word p_type;
  Elf64_Word p_flags;
  Elf64_Off p_offset;
  Elf64_Addr p_vaddr;
  Elf64_Addr p_paddr;
  Elf64_Xword p_filesz;
  Elf64_Xword p_memsz;
  Elf64_Xword p_align;
} Elf64_Phdr;

typedef union Elf32_Dyn_Union {
  Elf32_Word d_val;
  Elf32_Addr d_ptr;
} Elf32_Dyn_Union;

/**
 * See <https://www.man7.org/linux/man-pages/man5/elf.5.html>.
 */
typedef struct Elf32_Dyn {
  Elf32_Sword d_tag;
  union Elf32_Dyn_Union d_un;
} Elf32_Dyn;

typedef union Elf64_Dyn_Union {
  Elf64_Xword d_val;
  Elf64_Addr d_ptr;
} Elf64_Dyn_Union;

/**
 * See <https://www.man7.org/linux/man-pages/man5/elf.5.html>.
 */
typedef struct Elf64_Dyn {
  Elf64_Sxword d_tag;
  union Elf64_Dyn_Union d_un;
} Elf64_Dyn;

typedef struct Elf32_Verdef {
  Elf32_Half vd_version;
  Elf32_Half vd_flags;
  Elf32_Half vd_ndx;
  Elf32_Half vd_cnt;
  Elf32_Word vd_hash;
  Elf32_Word vd_aux;
  Elf32_Word vd_next;
} Elf32_Verdef;

typedef struct Elf64_Verdef {
  Elf64_Half vd_version;
  Elf64_Half vd_flags;
  Elf64_Half vd_ndx;
  Elf64_Half vd_cnt;
  Elf64_Word vd_hash;
  Elf64_Word vd_aux;
  Elf64_Word vd_next;
} Elf64_Verdef;

typedef struct Elf32_Verdaux {
  Elf32_Word vda_name;
  Elf32_Word vda_next;
} Elf32_Verdaux;

typedef struct Elf64_Verdaux {
  Elf64_Word vda_name;
  Elf64_Word vda_next;
} Elf64_Verdaux;

typedef struct Elf32_Verneed {
  Elf32_Half vn_version;
  Elf32_Half vn_cnt;
  Elf32_Word vn_file;
  Elf32_Word vn_aux;
  Elf32_Word vn_next;
} Elf32_Verneed;

typedef struct Elf64_Verneed {
  Elf64_Half vn_version;
  Elf64_Half vn_cnt;
  Elf64_Word vn_file;
  Elf64_Word vn_aux;
  Elf64_Word vn_next;
} Elf64_Verneed;

typedef struct Elf64_Vernaux {
  Elf64_Word vna_hash;
  Elf64_Half vna_flags;
  Elf64_Half vna_other;
  Elf64_Word vna_name;
  Elf64_Word vna_next;
} Elf64_Vernaux;

typedef union A_UN {
  uint64_t a_val;
} A_UN;

typedef struct Elf64_auxv_t {
  uint64_t a_type;
  union A_UN a_un;
} Elf64_auxv_t;

/**
 * See <https://www.man7.org/linux/man-pages/man5/elf.5.html>.
 */
typedef struct Elf32_Nhdr {
  Elf32_Word n_namesz;
  Elf32_Word n_descsz;
  Elf32_Word n_type;
} Elf32_Nhdr;

/**
 * See <https://www.man7.org/linux/man-pages/man5/elf.5.html>.
 */
typedef struct Elf64_Nhdr {
  Elf64_Word n_namesz;
  Elf64_Word n_descsz;
  Elf64_Word n_type;
} Elf64_Nhdr;

typedef struct Elf64_Move {
  Elf64_Xword m_value;
  Elf64_Xword m_info;
  Elf64_Xword m_poffset;
  Elf64_Half m_repeat;
  Elf64_Half m_stride;
} Elf64_Move;

#endif  /* _ELF_H */
