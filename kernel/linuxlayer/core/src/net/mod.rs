// SPDX-License-Identifier: MPL-2.0

// User space, 0.7.0: of the network, only the UTS namespace (the host and domain
// names) is carried. Sockets stay out (data-os.md §4).
pub(crate) mod uts_ns;
