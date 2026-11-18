#ifndef _RELIBC_BITS_WCHAR_T_H
#define _RELIBC_BITS_WCHAR_T_H

// Integer type whose range of values can represent distinct codes for all
// members of the largest extended character set specified among the supported
// locales; the null character shall have the code value zero.
#ifndef _WCHAR_T
#define _WCHAR_T
#ifndef __cplusplus
    #ifndef __WCHAR_TYPE__
        #define __WCHAR_TYPE__ int
    #endif
    typedef __WCHAR_TYPE__ wchar_t;
#endif // __cplusplus
#endif // _WCHAR_T


#endif  /* _RELIBC_BITS_WCHAR_T_H */
