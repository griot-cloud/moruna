//! SDK transport for the canonical control codec, not another storage request format.
use moruna_svc::{Channel, Handle, Memory};
moruna_svc::protocol! {
    /// Scoped native object listener. Caller retains originals and transfers only loans.
    pub protocol object {
        name="moruna.storage";
        major=1;
        minor=0;
        requests {
            /// Execute canonical storage-format::wire control against caller memory.
            /// `payload_offset` addresses the charged payload loan; reply is canonical
            /// Response bytes, returned length is exact. Object errors are typed in that
            /// Response; malformed/unmappable transport memory refuses before any IO.
            1 => fn execute(control:Memory, control_bytes:u32, payload:Option<Memory>, payload_offset:u64, reply:Memory, reply_capacity:u32) -> (u32,u64);
            /// Return canonical auth::Session facts bound at actual private admission.
            /// The public caller cannot choose or mutate the scope/backend role.
            2 => fn session(reply:Memory, reply_capacity:u32) -> (u32,Handle);
            /// Open an actual local durable namespace using canonical bounded control.
            /// Return a distinct already-bound namespace channel under this live context.
            3 => fn namespace(control:Memory, control_bytes:u32) -> Channel;
        }
        events {}
        errors {
            /// Wrong handle rights, backing length, offset or control/reply capacity.
            1 => BadMemory;
            /// The exact control envelope cannot be parsed or exceeds its bound.
            2 => BadControl;
            /// Required service/account/provider unavailable, without fallback.
            3 => Unavailable;
            /// Live exact-action verifier denied opening the requested namespace.
            4 => Denied;
            /// Actual configured provider cannot serve this operation/role.
            5 => Unsupported;
            /// Explicit resource admission budget exhausted before IO.
            6 => Capacity;
            /// Healthy owner already has another admitted transaction.
            7 => Busy;
        }
    }
}

// This is intentionally a different protocol, never an object-port operation. The
// endpoint itself is an attenuated boot/application authority, not a public listener.
moruna_svc::protocol! {
    /// Private designated authority for actual Scope + child Account session admission.
    pub protocol admission {
        name="moruna.storage.admission";
        major=1;
        minor=0;
        requests {
            /// Validate sole auth::Admission control, actual account object/rights and
            /// caller authority; return an already bound connected storage channel.
            /// The supplied account is the actual child account, never a parent alias.
            1 => fn bind(control:Memory, control_bytes:u32, account:Handle) -> Channel;
        }
        events {}
        errors {
            /// Malformed control, wrong handle type, or unsupported role.
            1 => Invalid;
            /// Exact live admission authority refuses the requested account/scope/role.
            2 => Denied;
            /// Required designated authority or configured backend unavailable.
            3 => Unavailable;
        }
    }
}

moruna_svc::protocol! {
    /// Actual application/boot-designated authority delegation. The returned channel is
    /// bound by that authority to the exact supplied child Account and admitted Scope.
    /// It speaks canonical auth::Authorization/Verdict bytes, never object operations.
    pub protocol delegate {
        name="moruna.storage.authority";
        major=1;
        minor=0;
        requests {
            /// Authorize/attenuate canonical Admission using the real account capability;
            /// return a live per-session exact-action verifier endpoint. No issuer/token
            /// format is synthesized by Moruna and missing authority must refuse.
            1 => fn bind(control:Memory, control_bytes:u32, account:Handle) -> Channel;
        }
        events {}
        errors {
            /// Invalid canonical control or actual account object/rights.
            1 => Invalid;
            /// The designated authority refused this admission/delegation.
            2 => Denied;
            /// Authority or required policy unavailable.
            3 => Unavailable;
        }
    }
}

moruna_svc::protocol! {
    /// Private boot-granted cache lifecycle control; never public object authority.
    pub protocol cache_admin {
        name="moruna.storage.cache-admin";
        major=1;
        minor=0;
        requests {
            /// Recycle only after actual cache-owned plaintext/ciphers/IO quiesce.
            /// Destroy epoch before clearing/syncing only the cache region journal.
            1 => fn recycle() -> ();
        }
        events {}
        errors {
            /// Actual cache users or owner borrow still live; nothing was recycled.
            1 => InUse;
            /// Real entropy, key or physical IO failed; no success/erase claim.
            2 => Unavailable;
        }
    }
}

moruna_svc::protocol! {
    /// Exclusive generic durable namespace, bound through actual object ordinal 3.
    /// Payload/cancellation loans never substitute for its admitted Account and Scope.
    pub protocol namespace {
        name="moruna.storage.namespace";major=1;minor=0;
        requests {
            /// Execute sole namespace::Request steps, preserving original Memory through
            /// actual read/write/sync completions and returning canonical Response.
            1 => fn execute(control:Memory,control_bytes:u32,payload:Option<Memory>,payload_offset:u64,cancel:Channel,reply:Memory,reply_capacity:u32)->(u32,u64);
            /// Actual admitted scope/account and published root receipt, not caller labels.
            2 => fn session(reply:Memory,reply_capacity:u32)->(u32,Handle);
        }
        events {}
        errors {
            /// Invalid actual origin/rights/backing/range of a Memory loan.
            1 => BadMemory;
            /// Invalid canonical namespace control or bounds.
            2 => BadControl;
            /// Real provider/authority/original-completion health unavailable.
            3 => Unavailable;
            /// Exact live authority refused the requested operation.
            4 => Denied;
            /// Explicit charged admission/capacity refusal.
            5 => Capacity;
            /// Existing lease, version or atomic rename condition conflicts.
            6 => Conflict;
            /// Actual provider cannot serve this operation or role.
            7 => Unsupported;
            /// Exact requested existing object/path is absent.
            8 => Missing;
            /// Healthy physical owner is executing another admitted original.
            9 => Busy;
        }
    }
}

moruna_svc::protocol! {
    /// Private boot-granted physical-owner housekeeping. No tenant listener, object
    /// roots, asserted Scope, caller Account or authority bytes occur on this protocol.
    pub protocol maintenance {
        name="moruna.storage.maintenance";major=1;minor=0;
        requests {
            /// Compact all configured durable local owners; acknowledge only after each
            /// actual copied journal/root has synced. Cache recycling is a separate port.
            1 => fn compact()->();
        }
        events {}
        errors {
            /// Explicit private capability/configuration unavailable.
            1 => Denied;
            /// Another real maintenance/original operation owns admission.
            2 => Busy;
            /// Explicit housekeeping budget cannot reserve required resources.
            3 => Capacity;
            /// Invalid canonical bounded policy or request shape.
            4 => Invalid;
            /// Actual device/owner/completion health unavailable.
            5 => Unavailable;
            /// Actual journal/data/root integrity refused.
            6 => Integrity;
        }
    }
}
