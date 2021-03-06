//! Intel Instruction Set Architectures.

pub mod settings;
mod enc_tables;
mod registers;

use super::super::settings as shared_settings;
use isa::enc_tables::{self as shared_enc_tables, lookup_enclist, general_encoding};
use isa::Builder as IsaBuilder;
use isa::{TargetIsa, RegInfo, Encoding, Legalize, RecipeConstraints};
use ir::{InstructionData, DataFlowGraph};

#[allow(dead_code)]
struct Isa {
    shared_flags: shared_settings::Flags,
    isa_flags: settings::Flags,
    cpumode: &'static [shared_enc_tables::Level1Entry<u16>],
}

/// Get an ISA builder for creating Intel targets.
pub fn isa_builder() -> IsaBuilder {
    IsaBuilder {
        setup: settings::builder(),
        constructor: isa_constructor,
    }
}

fn isa_constructor(shared_flags: shared_settings::Flags,
                   builder: &shared_settings::Builder)
                   -> Box<TargetIsa> {
    let level1 = if shared_flags.is_64bit() {
        &enc_tables::LEVEL1_I64[..]
    } else {
        &enc_tables::LEVEL1_I32[..]
    };
    Box::new(Isa {
        isa_flags: settings::Flags::new(&shared_flags, builder),
        shared_flags: shared_flags,
        cpumode: level1,
    })
}

impl TargetIsa for Isa {
    fn name(&self) -> &'static str {
        "intel"
    }

    fn flags(&self) -> &shared_settings::Flags {
        &self.shared_flags
    }

    fn register_info(&self) -> RegInfo {
        registers::INFO.clone()
    }

    fn encode(&self, dfg: &DataFlowGraph, inst: &InstructionData) -> Result<Encoding, Legalize> {
        lookup_enclist(inst.ctrl_typevar(dfg),
                       inst.opcode(),
                       self.cpumode,
                       &enc_tables::LEVEL2[..])
            .and_then(|enclist_offset| {
                general_encoding(enclist_offset,
                                 &enc_tables::ENCLISTS[..],
                                 |instp| enc_tables::check_instp(inst, instp),
                                 |isap| self.isa_flags.numbered_predicate(isap as usize))
                    .ok_or(Legalize::Expand)
            })
    }

    fn recipe_names(&self) -> &'static [&'static str] {
        &enc_tables::RECIPE_NAMES[..]
    }

    fn recipe_constraints(&self) -> &'static [RecipeConstraints] {
        &enc_tables::RECIPE_CONSTRAINTS
    }
}
