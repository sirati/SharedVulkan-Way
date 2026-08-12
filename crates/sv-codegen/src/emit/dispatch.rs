//! Passes 8–10: the opcode table, the client stubs, and the dispatchers.
//!
//! Per command the generator emits:
//!
//! - `VkFooArgs` — the decoded parameters, and the type a client stub is handed.
//! - `VkFooReply` — the result and the callee-filled parameters, when there is
//!   one. Most commands have none: design.md §4.4 follows Venus in generating a
//!   reply only when it is asked for.
//! - `size_cmd_*` / `encode_cmd_*` / `decode_cmd_*` — the app side's two passes
//!   and the GPU side's one.
//! - `size_reply_*` / `encode_reply_*` / `decode_reply_*` — the same, in the
//!   other direction.
//!
//! and, once for the whole protocol, the opcode enum, the `Server` trait, the
//! dispatcher, and the restricted sub-decoder of design.md §6.5.
//!
//! # Why output parameters appear in *both* directions
//!
//! Venus does this and it is not redundancy. An out-parameter's *storage* is
//! uninitialised on the way out, but its *shape* is not: `pPhysicalDevices` is
//! accompanied by the count the caller allocated for, and an object handle is
//! the id the app side has already chosen (design.md §6.4 — object creation
//! costs no round trip precisely because the id goes out with the request). So
//! the command carries the out-parameter in its `_partial` form and the reply
//! carries it in full.

use crate::naming;
use crate::plan::{CommandPlan, FieldPlan, PartialValidity, Plan, Shape, partial_validity};
use proc_macro2::TokenStream;
use quote::{format_ident, quote};

pub fn emit(plan: &Plan) -> TokenStream {
    let opcodes = emit_opcodes(plan);
    let per_command = plan.commands.iter().map(|c| emit_command(plan, c));
    let server = emit_server_trait(plan);
    let dispatcher = emit_dispatcher(plan);

    quote! {
        #opcodes
        #(#per_command)*
        #server
        #dispatcher
    }
}

fn args_ty(c: &CommandPlan) -> proc_macro2::Ident {
    format_ident!("{}Args", naming::opcode_variant(&c.name))
}
fn reply_ty(c: &CommandPlan) -> proc_macro2::Ident {
    format_ident!("{}Reply", naming::opcode_variant(&c.name))
}

fn emit_opcodes(plan: &Plan) -> TokenStream {
    let variants = plan.commands.iter().map(|c| {
        let v = naming::opcode_variant(&c.name);
        let n = c.opcode;
        let doc = format!(
            "`{}`{}{}",
            c.name,
            if c.aliases.is_empty() {
                String::new()
            } else {
                format!(" (also `{}`)", c.aliases.join("`, `"))
            },
            if c.origin.is_empty() { String::new() } else { format!(" — from {}", c.origin) }
        );
        quote! {
            #[doc = #doc]
            #v = #n,
        }
    });
    let from_arms = plan.commands.iter().map(|c| {
        let v = naming::opcode_variant(&c.name);
        let n = c.opcode;
        quote!(#n => Some(Opcode::#v),)
    });
    let name_arms = plan.commands.iter().map(|c| {
        let v = naming::opcode_variant(&c.name);
        let n = &c.name;
        quote!(Opcode::#v => #n,)
    });
    let recorded_arms = plan.commands.iter().filter(|c| c.is_recorded).map(|c| {
        let v = naming::opcode_variant(&c.name);
        quote!(Opcode::#v)
    });
    let recorded: Vec<_> = recorded_arms.collect();
    let recorded_body = if recorded.is_empty() {
        quote!(false)
    } else {
        quote!(matches!(self, #(#recorded)|*))
    };
    let count = plan.commands.len();

    quote! {
        /// The wire opcode of every command in this build's profile.
        ///
        /// The numbering is the sorted order of the command names, which is
        /// stable against the registry reordering a block but *not* against the
        /// profile gaining or losing a command. That is deliberate and it is
        /// safe: the profile is part of the protocol hash, so a build whose
        /// opcodes moved cannot connect to one whose did not (design.md §4.3,
        /// R13).
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        #[repr(u32)]
        pub enum Opcode {
            #(#variants)*
        }

        impl Opcode {
            pub const COUNT: usize = #count;

            pub fn from_u32(v: u32) -> Option<Opcode> {
                match v { #(#from_arms)* _ => None }
            }

            pub fn name(self) -> &'static str {
                match self { #(#name_arms)* }
            }

            /// Whether this opcode belongs to the recorded `vkCmd*` subset the
            /// restricted sub-decoder accepts (design.md §6.5).
            pub fn is_recorded(self) -> bool { #recorded_body }
        }
    }
}

fn emit_command(_plan: &Plan, c: &CommandPlan) -> TokenStream {
    let args = args_ty(c);
    let reply = reply_ty(c);
    let opcode = c.opcode;

    // -- the argument type ---------------------------------------------------
    let all_params: Vec<&FieldPlan> = c.inputs.iter().chain(c.outputs.iter()).collect();
    let arg_fields = all_params.iter().map(|f| {
        let id = naming::ident(&f.rust_name);
        let ty = super::rust_type(&f.shape);
        let doc = format!("`{}`", f.name);
        quote! { #[doc = #doc] pub #id: #ty, }
    });
    let arg_defaults = all_params.iter().map(|f| {
        let id = naming::ident(&f.rust_name);
        let d = super::default_expr(&f.shape);
        quote!(#id: #d,)
    });

    // Parameters are encoded in registry declaration order, which is Venus's
    // order and the order a differential comparison depends on.
    let param_mode = |f: &&FieldPlan| {
        if f.is_output && partial_validity(f) != PartialValidity::Full {
            super::wire::Mode::Partial
        } else {
            super::wire::Mode::Full
        }
    };
    let size_parts = all_params.iter().map(|f| {
        let id = naming::ident(&f.rust_name);
        let e = super::wire::size_expr(&f.shape, &quote!(a.#id), param_mode(f));
        quote!(n += #e;)
    });
    let encode_parts = all_params.iter().map(|f| {
        let id = naming::ident(&f.rust_name);
        super::wire::encode_stmt(&f.shape, &quote!(a.#id), param_mode(f))
    });
    let param_list: Vec<FieldPlan> = all_params.iter().map(|f| (*f).clone()).collect();
    let decode_parts = all_params.iter().map(|f| {
        let id = naming::ident(&f.rust_name);
        let bound = super::wire::count_bound(&param_list, f, &quote!(out));
        let e = super::wire::decode_expr_bounded(&f.shape, param_mode(f), bound.as_ref());
        quote!(out.#id = #e;)
    });

    let mut_n = if all_params.is_empty() { quote!() } else { quote!(mut) };
    let size_cmd = format_ident!("size_cmd_{}", naming::snake_case(&c.name));
    let encode_cmd = format_ident!("encode_cmd_{}", naming::snake_case(&c.name));
    let decode_cmd = format_ident!("decode_cmd_{}", naming::snake_case(&c.name));

    let args_doc = format!("Parameters of `{}`.", c.name);
    let stub_doc = format!(
        "Client stub for `{}`: bytes the encoded command will occupy, header \
         included.\n\n\
         The `size` and `encode` passes are separate because a ring span has to \
         be reserved before it can be written (design.md §7.2), and \
         [`sv_wire::Encoder::finish`] checks afterwards that they agreed.",
        c.name
    );

    // -- the reply type ------------------------------------------------------
    let reply_block = if c.generates_reply {
        let result_field = if c.returns_result {
            quote!(pub result: VkResult,)
        } else {
            quote!()
        };
        let result_default = if c.returns_result {
            quote!(result: VkResult(0),)
        } else {
            quote!()
        };
        let out_fields = c.outputs.iter().map(|f| {
            let id = naming::ident(&f.rust_name);
            let ty = super::rust_type(&f.shape);
            let doc = format!("`{}`, as the implementation filled it in", f.name);
            quote! { #[doc = #doc] pub #id: #ty, }
        });
        let out_defaults = c.outputs.iter().map(|f| {
            let id = naming::ident(&f.rust_name);
            let d = super::default_expr(&f.shape);
            quote!(#id: #d,)
        });
        let result_size = if c.returns_result { quote!(n += 4usize;) } else { quote!() };
        let result_encode = if c.returns_result { quote!(enc.i32(r.result.0)?;) } else { quote!() };
        let result_decode =
            if c.returns_result { quote!(out.result = VkResult(dec.i32()?);) } else { quote!() };

        let rsize = c.outputs.iter().map(|f| {
            let id = naming::ident(&f.rust_name);
            let e = super::wire::size_expr(&f.shape, &quote!(r.#id), super::wire::Mode::Full);
            quote!(n += #e;)
        });
        let rencode = c.outputs.iter().map(|f| {
            let id = naming::ident(&f.rust_name);
            super::wire::encode_stmt(&f.shape, &quote!(r.#id), super::wire::Mode::Full)
        });
        let rdecode = c.outputs.iter().map(|f| {
            let id = naming::ident(&f.rust_name);
            let e = super::wire::decode_expr(&f.shape, super::wire::Mode::Full);
            quote!(out.#id = #e;)
        });

        let mut_r = if c.returns_result || !c.outputs.is_empty() { quote!(mut) } else { quote!() };
        let size_reply = format_ident!("size_reply_{}", naming::snake_case(&c.name));
        let encode_reply = format_ident!("encode_reply_{}", naming::snake_case(&c.name));
        let decode_reply = format_ident!("decode_reply_{}", naming::snake_case(&c.name));
        let reply_doc = format!("What `{}` sends back.", c.name);

        quote! {
            #[doc = #reply_doc]
            #[derive(Debug, Clone, PartialEq)]
            pub struct #reply {
                #result_field
                #(#out_fields)*
            }

            impl Default for #reply {
                fn default() -> Self { Self { #result_default #(#out_defaults)* } }
            }

            pub fn #size_reply(r: &#reply) -> usize {
                let #mut_r n = sv_wire::REPLY_HEADER_SIZE;
                #result_size
                #(#rsize)*
                let _ = r;
                n
            }

            pub fn #encode_reply(
                enc: &mut sv_wire::Encoder<'_>,
                r: &#reply,
            ) -> sv_wire::Result<()> {
                sv_wire::ReplyHeader {
                    cmd_type: #opcode,
                    reply_size: #size_reply(r) as u32,
                }.encode(enc)?;
                #result_encode
                #(#rencode)*
                let _ = (&enc, r);
                Ok(())
            }

            pub fn #decode_reply(dec: &mut sv_wire::Decoder<'_>) -> sv_wire::Result<#reply> {
                let header = sv_wire::ReplyHeader::decode(dec)?;
                if header.cmd_type != #opcode {
                    return Err(sv_wire::WireError::UnknownCommand { opcode: header.cmd_type });
                }
                let #mut_r out = <#reply as Default>::default();
                #result_decode
                #(#rdecode)*
                let _ = &dec;
                Ok(out)
            }
        }
    } else {
        quote!()
    };

    quote! {
        #[doc = #args_doc]
        #[derive(Debug, Clone, PartialEq)]
        pub struct #args {
            #(#arg_fields)*
        }

        impl Default for #args {
            fn default() -> Self { Self { #(#arg_defaults)* } }
        }

        #[doc = #stub_doc]
        pub fn #size_cmd(a: &#args) -> usize {
            let #mut_n n = sv_wire::COMMAND_HEADER_SIZE;
            #(#size_parts)*
            let _ = a;
            n
        }

        pub fn #encode_cmd(
            enc: &mut sv_wire::Encoder<'_>,
            flags: u32,
            a: &#args,
        ) -> sv_wire::Result<()> {
            sv_wire::CommandHeader {
                cmd_type: #opcode,
                cmd_flags: flags,
                cmd_size: #size_cmd(a) as u32,
            }.encode(enc)?;
            #(#encode_parts)*
            let _ = (&enc, a);
            Ok(())
        }

        /// Read the parameters of an already-headered command.
        ///
        /// The header is consumed by [`dispatch`], which needs the opcode to
        /// know which of these to call.
        pub fn #decode_cmd(dec: &mut sv_wire::Decoder<'_>) -> sv_wire::Result<#args> {
            let #mut_n out = <#args as Default>::default();
            #(#decode_parts)*
            let _ = &dec;
            Ok(out)
        }

        #reply_block
    }
}

fn emit_server_trait(plan: &Plan) -> TokenStream {
    let methods = plan.commands.iter().map(|c| {
        let m = naming::command_method(&c.name);
        let args = args_ty(c);
        let opcode = c.opcode;
        let doc = format!(
            "Handle `{}`.\n\nThe default refuses with \
             [`sv_wire::WireError::UnknownCommand`] carrying this command's own \
             opcode, so a GPU side implements the commands it has and every \
             other one fails by name rather than failing to compile.",
            c.name
        );
        if c.generates_reply && c.returns_result {
            // A command that returns a `VkResult` has somewhere to put a
            // refusal, so the default **fails the call and not the session**.
            //
            // That distinction is the difference between a GPU side that can be
            // widened one command at a time and one that cannot. Returning `Err`
            // here propagates out of `dispatch`, which produces no reply at all
            // — so the ring goes fatal and the app side, still waiting, times
            // out on a command it will never hear about. One unimplemented query
            // would take the device with it, which is exactly what happened on
            // the first end-to-end run: `vulkaninfo` reached
            // `vkGetPhysicalDeviceImageFormatProperties` and lost the device.
            let reply = reply_ty(c);
            quote! {
                #[doc = #doc]
                fn #m(&mut self, args: #args) -> sv_wire::Result<#reply> {
                    let _ = args;
                    let mut r = <#reply as Default>::default();
                    // `VK_ERROR_INITIALIZATION_FAILED`, for the reason
                    // `sv_icd_entry::lower_failed` gives: it is a result almost
                    // every command may return, so an application that handles
                    // errors at all handles it.
                    r.result = VkResult(-3);
                    Ok(r)
                }
            }
        } else if c.generates_reply {
            // A reply with no `VkResult` — `vkGetBufferMemoryRequirements` and
            // its kind. There is nowhere to put a refusal and inventing zeros
            // would be worse than failing, because the caller cannot tell them
            // apart from a real answer.
            let reply = reply_ty(c);
            quote! {
                #[doc = #doc]
                fn #m(&mut self, args: #args) -> sv_wire::Result<#reply> {
                    let _ = args;
                    Err(sv_wire::WireError::UnknownCommand { opcode: #opcode })
                }
            }
        } else {
            quote! {
                #[doc = #doc]
                fn #m(&mut self, args: #args) -> sv_wire::Result<()> {
                    let _ = args;
                    Err(sv_wire::WireError::UnknownCommand { opcode: #opcode })
                }
            }
        }
    });

    quote! {
        /// What the GPU side implements.
        ///
        /// One method per command, taking decoded, owned arguments and returning
        /// a decoded reply. Everything about the wire is on this side of the
        /// trait; nothing about Vulkan is. That separation is what lets the
        /// whole protocol be tested against a fake server with no driver
        /// present, which is what design.md §13 steps 1–6 need.
        ///
        /// # Every method has a default, and that is a correction to design.md
        ///
        /// §13 sequences the GPU side as a widening series of steps — step 4 is
        /// a byte-pattern round trip, step 6 is "widen to Zink's requirement
        /// set" — and that sequence is only expressible if a `Server` may
        /// implement a *subset*. As first generated this trait had 239 required
        /// methods, so the smallest possible GPU side was 239 stubs before it
        /// could answer one command, and each new step would have edited stubs
        /// rather than added methods. The refusal is the dual of the `Icd`
        /// trait's, which had defaults from the start; the asymmetry was an
        /// oversight rather than a decision.
        pub trait Server {
            #(#methods)*
        }
    }
}

fn emit_dispatcher(plan: &Plan) -> TokenStream {
    let arms = plan.commands.iter().map(|c| {
        let v = naming::opcode_variant(&c.name);
        let m = naming::command_method(&c.name);
        let decode_cmd = format_ident!("decode_cmd_{}", naming::snake_case(&c.name));
        if c.generates_reply {
            let encode_reply = format_ident!("encode_reply_{}", naming::snake_case(&c.name));
            let size_reply = format_ident!("size_reply_{}", naming::snake_case(&c.name));
            quote! {
                Opcode::#v => {
                    let args = #decode_cmd(&mut body)?;
                    let r = srv.#m(args)?;
                    if header.wants_reply() {
                        let n = #size_reply(&r);
                        let at = reply.len();
                        reply.resize(at + n, 0);
                        let mut enc = sv_wire::Encoder::new(&mut reply[at..]);
                        #encode_reply(&mut enc, &r)?;
                        enc.finish()?;
                    }
                }
            }
        } else {
            quote! {
                Opcode::#v => {
                    let args = #decode_cmd(&mut body)?;
                    srv.#m(args)?;
                }
            }
        }
    });

    let sub_arms = plan.commands.iter().filter(|c| c.is_recorded).map(|c| {
        let v = naming::opcode_variant(&c.name);
        let m = naming::command_method(&c.name);
        let decode_cmd = format_ident!("decode_cmd_{}", naming::snake_case(&c.name));
        quote! {
            Opcode::#v => {
                let args = #decode_cmd(&mut body)?;
                srv.#m(args)?;
            }
        }
    });

    quote! {
        /// Decode one command and hand it to the server.
        ///
        /// The header's `cmd_size` is checked against what decoding actually
        /// consumed. That check is the whole reason design.md §6.2 spends four
        /// bytes per command: without it, two builds from different registries
        /// do not fail to connect — they desynchronise and misinterpret
        /// everything downstream (R13). With it, the very first mismatched
        /// command is an error carrying its own opcode.
        pub fn dispatch(
            dec: &mut sv_wire::Decoder<'_>,
            srv: &mut dyn Server,
            reply: &mut Vec<u8>,
        ) -> sv_wire::Result<Opcode> {
            let header = sv_wire::CommandHeader::decode(dec)?;
            let opcode = Opcode::from_u32(header.cmd_type)
                .ok_or(sv_wire::WireError::UnknownCommand { opcode: header.cmd_type })?;
            let body_len = (header.cmd_size as usize)
                .checked_sub(sv_wire::COMMAND_HEADER_SIZE)
                .ok_or(sv_wire::WireError::CommandLengthMismatch {
                    declared: header.cmd_size,
                    consumed: sv_wire::COMMAND_HEADER_SIZE,
                })?;
            let mut body = dec.sub(body_len)?;

            match opcode {
                #(#arms)*
            }

            if body.remaining() != 0 {
                return Err(sv_wire::WireError::CommandLengthMismatch {
                    declared: header.cmd_size,
                    consumed: header.cmd_size as usize - body.remaining(),
                });
            }
            Ok(opcode)
        }

        /// Decode every command in a recorded blob.
        ///
        /// The restricted sub-decoder of design.md §6.5: it accepts only the
        /// `vkCmd*` opcodes, so a blob that names anything else is rejected
        /// rather than executed. gfxstream's `vk_sub_decoder.cpp` and Venus's
        /// `vkExecuteCommandStreamsMESA` do the same, for the same reason —
        /// a recorded blob is application-controlled bytes and the set of things
        /// it is allowed to say is much smaller than the whole protocol.
        pub fn subdecode(
            dec: &mut sv_wire::Decoder<'_>,
            srv: &mut dyn Server,
        ) -> sv_wire::Result<usize> {
            let mut count = 0usize;
            while !dec.is_empty() {
                let header = sv_wire::CommandHeader::decode(dec)?;
                let opcode = Opcode::from_u32(header.cmd_type)
                    .ok_or(sv_wire::WireError::UnknownCommand { opcode: header.cmd_type })?;
                if !opcode.is_recorded() {
                    return Err(sv_wire::WireError::UnknownCommand { opcode: header.cmd_type });
                }
                let body_len = (header.cmd_size as usize)
                    .checked_sub(sv_wire::COMMAND_HEADER_SIZE)
                    .ok_or(sv_wire::WireError::CommandLengthMismatch {
                        declared: header.cmd_size,
                        consumed: sv_wire::COMMAND_HEADER_SIZE,
                    })?;
                let mut body = dec.sub(body_len)?;
                match opcode {
                    #(#sub_arms)*
                    _ => return Err(sv_wire::WireError::UnknownCommand {
                        opcode: header.cmd_type,
                    }),
                }
                if body.remaining() != 0 {
                    return Err(sv_wire::WireError::CommandLengthMismatch {
                        declared: header.cmd_size,
                        consumed: header.cmd_size as usize - body.remaining(),
                    });
                }
                count += 1;
            }
            Ok(count)
        }
    }
}

/// Whether a command's parameter list mentions `pNext` anywhere, which the test
/// generator uses to pick interesting samples.
pub fn touches_pnext(c: &CommandPlan) -> bool {
    c.inputs.iter().chain(c.outputs.iter()).any(|f| matches!(f.shape, Shape::PNext))
}
