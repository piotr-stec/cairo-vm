use crate::{
    serde::{
        deserialize_program::{parse_program_json, ProgramJson},
        serialize_program::ProgramSerializer,
    },
    stdlib::{
        collections::{BTreeMap, HashMap},
        prelude::*,
        sync::Arc,
    },
    vm::runners::cairo_pie::StrippedProgram,
};

#[cfg(feature = "cairo-1-hints")]
use crate::serde::deserialize_program::{ApTracking, FlowTrackingData};
use crate::{
    hint_processor::hint_processor_definition::HintReference,
    serde::deserialize_program::{
        deserialize_and_parse_program, Attribute, BuiltinName, HintParams, Identifier,
        InstructionLocation, OffsetValue, ReferenceManager,
    },
    types::{
        errors::program_errors::ProgramError, instruction::Register, relocatable::MaybeRelocatable,
    },
};
#[cfg(feature = "cairo-1-hints")]
use cairo_lang_starknet::casm_contract_class::CasmContractClass;
use core::num::NonZeroUsize;
use felt::{Felt252, PRIME_STR};
use serde::{Deserialize, Serialize};

#[cfg(feature = "std")]
use std::path::Path;

#[cfg(all(feature = "arbitrary", feature = "std"))]
use arbitrary::{Arbitrary, Unstructured};

// NOTE: `Program` has been split in two containing some data that will be deep-copied
// and some that will be allocated on the heap inside an `Arc<_>`.
// This is because it has been reported that cloning the whole structure when creating
// a `CairoRunner` becomes a bottleneck, but the following solutions were tried and
// discarded:
// - Store only a reference in `CairoRunner` rather than cloning; this doesn't work
//   because then we need to introduce explicit lifetimes, which broke `cairo-vm-py`
//   since PyO3 doesn't support Python objects containing structures with lifetimes.
// - Directly pass an `Arc<Program>` to `CairoRunner::new()` and simply copy that:
//   there was a prohibitive performance hit of 10-15% when doing so, most likely
//   either because of branch mispredictions or the extra level of indirection going
//   through a random location on the heap rather than the likely-to-be-cached spot
//   on the stack.
//
// So, the compromise was to identify which data was less used and avoid copying that,
// using `Arc<_>`, while the most accessed fields remain on the stack for the main
// loop to access. The fields in `SharedProgramData` are either preprocessed and
// copied explicitly (_in addition_ to the clone of `Program`) or are used only in
// exceptional circumstances, such as when reconstructing a backtrace on execution
// failures.
// Fields in `Program` (other than `SharedProgramData` itself) are used by the main logic.
#[derive(Clone, Default, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SharedProgramData {
    pub(crate) data: Vec<MaybeRelocatable>,
    pub(crate) hints_collection: HintsCollection,
    pub(crate) main: Option<usize>,
    //start and end labels will only be used in proof-mode
    pub(crate) start: Option<usize>,
    pub(crate) end: Option<usize>,
    pub(crate) error_message_attributes: Vec<Attribute>,
    pub(crate) instruction_locations: Option<HashMap<usize, InstructionLocation>>,
    #[serde(skip_serializing, skip_deserializing)]
    pub(crate) identifiers: HashMap<String, Identifier>,
    pub(crate) reference_manager: Vec<HintReference>,
}

#[cfg(all(feature = "arbitrary", feature = "std"))]
impl<'a> Arbitrary<'a> for SharedProgramData {
    /// Create an arbitary [`SharedProgramData`] using `HintsCollection::new` to generate `hints` and
    /// `hints_ranges`
    fn arbitrary(u: &mut Unstructured<'a>) -> arbitrary::Result<Self> {
        let mut data = Vec::new();
        let len = usize::arbitrary(u)?;
        for i in 0..len {
            let instruction = u64::arbitrary(u)?;
            data.push(MaybeRelocatable::from(Felt252::from(instruction)));
            // Check if the Imm flag is on and add an immediate value if it is
            if instruction & 0x0004000000000000 != 0 && i < len - 1 {
                data.push(MaybeRelocatable::from(Felt252::arbitrary(u)?));
            }
        }

        let raw_hints = BTreeMap::<usize, Vec<HintParams>>::arbitrary(u)?;
        let hints_collection = HintsCollection::new(&raw_hints, data.len())
            .map_err(|_| arbitrary::Error::IncorrectFormat)?;
        Ok(SharedProgramData {
            data,
            hints_collection,
            main: Option::<usize>::arbitrary(u)?,
            start: Option::<usize>::arbitrary(u)?,
            end: Option::<usize>::arbitrary(u)?,
            error_message_attributes: Vec::<Attribute>::arbitrary(u)?,
            instruction_locations: Option::<HashMap<usize, InstructionLocation>>::arbitrary(u)?,
            identifiers: HashMap::<String, Identifier>::arbitrary(u)?,
            reference_manager: Vec::<HintReference>::arbitrary(u)?,
        })
    }
}

#[derive(Clone, Default, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct HintsCollection {
    hints: Vec<HintParams>,
    /// This maps a PC to the range of hints in `hints` that correspond to it.
    hints_ranges: Vec<HintRange>,
}

impl HintsCollection {
    pub(crate) fn new(
        hints: &BTreeMap<usize, Vec<HintParams>>,
        program_length: usize,
    ) -> Result<Self, ProgramError> {
        let bounds = hints
            .iter()
            .map(|(pc, hs)| (*pc, hs.len()))
            .reduce(|(max_hint_pc, full_len), (pc, len)| (max_hint_pc.max(pc), full_len + len));

        let Some((max_hint_pc, full_len)) = bounds else {
            return Ok(HintsCollection {
                hints: Vec::new(),
                hints_ranges: Vec::new(),
            });
        };

        if max_hint_pc >= program_length {
            return Err(ProgramError::InvalidHintPc(max_hint_pc, program_length));
        }

        let mut hints_values = Vec::with_capacity(full_len);
        let mut hints_ranges = vec![None; max_hint_pc + 1];

        for (pc, hs) in hints.iter().filter(|(_, hs)| !hs.is_empty()) {
            let range = (
                hints_values.len(),
                NonZeroUsize::new(hs.len()).expect("empty vecs already filtered"),
            );
            hints_ranges[*pc] = Some(range);
            hints_values.extend_from_slice(&hs[..]);
        }

        Ok(HintsCollection {
            hints: hints_values,
            hints_ranges,
        })
    }

    pub fn iter_hints(&self) -> impl Iterator<Item = &HintParams> {
        self.hints.iter()
    }

    pub fn get_hint_range_for_pc(&self, pc: usize) -> Option<HintRange> {
        self.hints_ranges.get(pc).cloned()
    }
}

impl From<&HintsCollection> for BTreeMap<usize, Vec<HintParams>> {
    fn from(hc: &HintsCollection) -> Self {
        let mut hint_map = BTreeMap::new();
        for (i, r) in hc.hints_ranges.iter().enumerate() {
            let Some(r) = r else {
                continue;
            };
            hint_map.insert(i, hc.hints[r.0..r.0 + r.1.get()].to_owned());
        }
        hint_map
    }
}

/// Represents a range of hints corresponding to a PC.
///
/// Is [`None`] if the range is empty, and it is [`Some`] tuple `(start, length)` otherwise.
type HintRange = Option<(usize, NonZeroUsize)>;

#[cfg_attr(all(feature = "arbitrary", feature = "std"), derive(Arbitrary))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Program {
    #[serde(default)]
    pub(crate) shared_program_data: Arc<SharedProgramData>,
    #[serde(default)]
    pub(crate) constants: HashMap<String, Felt252>,
    #[serde(default)]
    pub(crate) builtins: Vec<BuiltinName>,
}

impl Program {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        builtins: Vec<BuiltinName>,
        data: Vec<MaybeRelocatable>,
        main: Option<usize>,
        hints: HashMap<usize, Vec<HintParams>>,
        reference_manager: ReferenceManager,
        identifiers: HashMap<String, Identifier>,
        error_message_attributes: Vec<Attribute>,
        instruction_locations: Option<HashMap<usize, InstructionLocation>>,
    ) -> Result<Program, ProgramError> {
        let constants = Self::extract_constants(&identifiers)?;

        let hints: BTreeMap<_, _> = hints.into_iter().collect();
        let hints_collection = HintsCollection::new(&hints, data.len())?;

        let shared_program_data = SharedProgramData {
            data,
            main,
            start: None,
            end: None,
            hints_collection,
            error_message_attributes,
            instruction_locations,
            identifiers,
            reference_manager: Self::get_reference_list(&reference_manager),
        };
        Ok(Self {
            shared_program_data: Arc::new(shared_program_data),
            constants,
            builtins,
        })
    }
    #[allow(clippy::too_many_arguments)]
    pub fn new_for_proof(
        builtins: Vec<BuiltinName>,
        data: Vec<MaybeRelocatable>,
        start: usize,
        end: usize,
        hints: HashMap<usize, Vec<HintParams>>,
        reference_manager: ReferenceManager,
        identifiers: HashMap<String, Identifier>,
        error_message_attributes: Vec<Attribute>,
        instruction_locations: Option<HashMap<usize, InstructionLocation>>,
    ) -> Result<Program, ProgramError> {
        let constants = Self::extract_constants(&identifiers)?;

        let hints: BTreeMap<_, _> = hints.into_iter().collect();
        let hints_collection = HintsCollection::new(&hints, data.len())?;

        let shared_program_data = SharedProgramData {
            data,
            main: None,
            start: Some(start),
            end: Some(end),
            hints_collection,
            error_message_attributes,
            instruction_locations,
            identifiers,
            reference_manager: Self::get_reference_list(&reference_manager),
        };
        Ok(Self {
            shared_program_data: Arc::new(shared_program_data),
            constants,
            builtins,
        })
    }

    #[cfg(feature = "std")]
    pub fn from_file(path: &Path, entrypoint: Option<&str>) -> Result<Program, ProgramError> {
        let file_content = std::fs::read(path)?;
        deserialize_and_parse_program(&file_content, entrypoint)
    }

    pub fn from_bytes(bytes: &[u8], entrypoint: Option<&str>) -> Result<Program, ProgramError> {
        deserialize_and_parse_program(bytes, entrypoint)
    }

    pub fn prime(&self) -> &str {
        _ = self;
        PRIME_STR
    }

    pub fn iter_builtins(&self) -> impl Iterator<Item = &BuiltinName> {
        self.builtins.iter()
    }

    pub fn iter_data(&self) -> impl Iterator<Item = &MaybeRelocatable> {
        self.shared_program_data.data.iter()
    }

    pub fn data_len(&self) -> usize {
        self.shared_program_data.data.len()
    }

    pub fn builtins_len(&self) -> usize {
        self.builtins.len()
    }

    pub fn get_identifier(&self, id: &str) -> Option<&Identifier> {
        self.shared_program_data.identifiers.get(id)
    }

    pub fn iter_identifiers(&self) -> impl Iterator<Item = (&str, &Identifier)> {
        self.shared_program_data
            .identifiers
            .iter()
            .map(|(cairo_type, identifier)| (cairo_type.as_str(), identifier))
    }

    pub(crate) fn get_reference_list(reference_manager: &ReferenceManager) -> Vec<HintReference> {
        reference_manager
            .references
            .iter()
            .map(|r| {
                HintReference {
                    offset1: r.value_address.offset1.clone(),
                    offset2: r.value_address.offset2.clone(),
                    dereference: r.value_address.dereference,
                    // only store `ap` tracking data if the reference is referred to it
                    ap_tracking_data: match (&r.value_address.offset1, &r.value_address.offset2) {
                        (OffsetValue::Reference(Register::AP, _, _), _)
                        | (_, OffsetValue::Reference(Register::AP, _, _)) => {
                            Some(r.ap_tracking_data.clone())
                        }
                        _ => None,
                    },
                    cairo_type: Some(r.value_address.value_type.clone()),
                }
            })
            .collect()
    }

    pub(crate) fn extract_constants(
        identifiers: &HashMap<String, Identifier>,
    ) -> Result<HashMap<String, Felt252>, ProgramError> {
        let mut constants = HashMap::new();
        for (key, value) in identifiers.iter() {
            if value.type_.as_deref() == Some("const") {
                let value = value
                    .value
                    .clone()
                    .ok_or_else(|| ProgramError::ConstWithoutValue(key.clone()))?;
                constants.insert(key.clone(), value);
            }
        }
        Ok(constants)
    }

    // Obtains a reduced version of the program
    // Doesn't contain hints
    // Can be used for verifying execution.
    pub fn get_stripped_program(&self) -> Result<StrippedProgram, ProgramError> {
        Ok(StrippedProgram {
            data: self.shared_program_data.data.clone(),
            builtins: self.builtins.clone(),
            main: self
                .shared_program_data
                .main
                .ok_or(ProgramError::StrippedProgramNoMain)?,
            prime: (),
        })
    }

    pub fn serialize(&self) -> Result<Vec<u8>, ProgramError> {
        let program_serializer: ProgramSerializer = ProgramSerializer::from(self);
        let bytes: Vec<u8> = serde_json::to_vec(&program_serializer)?;
        Ok(bytes)
    }

    pub fn deserialize(
        program_serializer_bytes: &[u8],
        entrypoint: Option<&str>,
    ) -> Result<Program, ProgramError> {
        let program_serializer: ProgramSerializer =
            serde_json::from_slice(program_serializer_bytes)?;
        let program_json = ProgramJson::from(program_serializer);
        let program = parse_program_json(program_json, entrypoint)?;
        Ok(program)
    }
}

impl Default for Program {
    fn default() -> Self {
        Self {
            shared_program_data: Arc::new(SharedProgramData::default()),
            constants: HashMap::new(),
            builtins: Vec::new(),
        }
    }
}

#[cfg(feature = "cairo-1-hints")]
// Note: This Program will only work when using run_from_entrypoint, and the Cairo1Hintprocesso
impl TryFrom<CasmContractClass> for Program {
    type Error = ProgramError;
    fn try_from(value: CasmContractClass) -> Result<Self, ProgramError> {
        let data = value
            .bytecode
            .iter()
            .map(|x| MaybeRelocatable::from(Felt252::from(x.value.clone())))
            .collect();
        //Hint data is going to be hosted processor-side, hints field will only store the pc where hints are located.
        // Only one pc will be stored, so the hint processor will be responsible for executing all hints for a given pc
        let hints = value
            .hints
            .iter()
            .map(|(x, _)| {
                (
                    *x,
                    vec![HintParams {
                        code: x.to_string(),
                        accessible_scopes: Vec::new(),
                        flow_tracking_data: FlowTrackingData {
                            ap_tracking: ApTracking::default(),
                            reference_ids: HashMap::new(),
                        },
                    }],
                )
            })
            .collect();
        let error_message_attributes = Vec::new();
        let reference_manager = ReferenceManager {
            references: Vec::new(),
        };
        Self::new(
            vec![],
            data,
            None,
            hints,
            reference_manager,
            HashMap::new(),
            error_message_attributes,
            None,
        )
    }
}

#[cfg(test)]
impl HintsCollection {
    pub fn iter(&self) -> impl Iterator<Item = (usize, &[HintParams])> {
        self.hints_ranges
            .iter()
            .enumerate()
            .filter_map(|(pc, range)| {
                range.and_then(|(start, len)| {
                    let end = start + len.get();
                    if end <= self.hints.len() {
                        Some((pc, &self.hints[start..end]))
                    } else {
                        None
                    }
                })
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serde::deserialize_program::{ApTracking, FlowTrackingData};
    use crate::utils::test_utils::*;
    use felt::felt_str;
    use num_traits::Zero;

    use assert_matches::assert_matches;

    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::*;

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn new() {
        let reference_manager = ReferenceManager {
            references: Vec::new(),
        };

        let builtins: Vec<BuiltinName> = Vec::new();
        let data: Vec<MaybeRelocatable> = vec![
            mayberelocatable!(5189976364521848832),
            mayberelocatable!(1000),
            mayberelocatable!(5189976364521848832),
            mayberelocatable!(2000),
            mayberelocatable!(5201798304953696256),
            mayberelocatable!(2345108766317314046),
        ];

        let program = Program::new(
            builtins.clone(),
            data.clone(),
            None,
            HashMap::new(),
            reference_manager,
            HashMap::new(),
            Vec::new(),
            None,
        )
        .unwrap();

        assert_eq!(program.builtins, builtins);
        assert_eq!(program.shared_program_data.data, data);
        assert_eq!(program.shared_program_data.main, None);
        assert_eq!(program.shared_program_data.identifiers, HashMap::new());
        assert_eq!(
            program.shared_program_data.hints_collection.hints,
            Vec::new()
        );
        assert_eq!(
            program.shared_program_data.hints_collection.hints_ranges,
            Vec::new()
        );
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn new_for_proof() {
        let reference_manager = ReferenceManager {
            references: Vec::new(),
        };

        let builtins: Vec<BuiltinName> = Vec::new();
        let data: Vec<MaybeRelocatable> = vec![
            mayberelocatable!(5189976364521848832),
            mayberelocatable!(1000),
            mayberelocatable!(5189976364521848832),
            mayberelocatable!(2000),
            mayberelocatable!(5201798304953696256),
            mayberelocatable!(2345108766317314046),
        ];

        let program = Program::new_for_proof(
            builtins.clone(),
            data.clone(),
            0,
            1,
            HashMap::new(),
            reference_manager,
            HashMap::new(),
            Vec::new(),
            None,
        )
        .unwrap();

        assert_eq!(program.builtins, builtins);
        assert_eq!(program.shared_program_data.data, data);
        assert_eq!(program.shared_program_data.main, None);
        assert_eq!(program.shared_program_data.start, Some(0));
        assert_eq!(program.shared_program_data.end, Some(1));
        assert_eq!(program.shared_program_data.identifiers, HashMap::new());
        assert_eq!(
            program.shared_program_data.hints_collection.hints,
            Vec::new()
        );
        assert_eq!(
            program.shared_program_data.hints_collection.hints_ranges,
            Vec::new()
        );
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn new_program_with_hints() {
        let reference_manager = ReferenceManager {
            references: Vec::new(),
        };

        let builtins: Vec<BuiltinName> = Vec::new();
        let data: Vec<MaybeRelocatable> = vec![
            mayberelocatable!(5189976364521848832),
            mayberelocatable!(1000),
            mayberelocatable!(5189976364521848832),
            mayberelocatable!(2000),
            mayberelocatable!(5201798304953696256),
            mayberelocatable!(2345108766317314046),
        ];

        let str_to_hint_param = |s: &str| HintParams {
            code: s.to_string(),
            accessible_scopes: vec![],
            flow_tracking_data: FlowTrackingData {
                ap_tracking: ApTracking {
                    group: 0,
                    offset: 0,
                },
                reference_ids: HashMap::new(),
            },
        };

        let hints = HashMap::from([
            (5, vec![str_to_hint_param("c"), str_to_hint_param("d")]),
            (1, vec![str_to_hint_param("a")]),
            (4, vec![str_to_hint_param("b")]),
        ]);

        let program = Program::new(
            builtins.clone(),
            data.clone(),
            None,
            hints.clone(),
            reference_manager,
            HashMap::new(),
            Vec::new(),
            None,
        )
        .unwrap();

        assert_eq!(program.builtins, builtins);
        assert_eq!(program.shared_program_data.data, data);
        assert_eq!(program.shared_program_data.main, None);
        assert_eq!(program.shared_program_data.identifiers, HashMap::new());

        let program_hints: HashMap<_, _> = program
            .shared_program_data
            .hints_collection
            .hints_ranges
            .iter()
            .enumerate()
            .filter_map(|(pc, r)| r.map(|(s, l)| (pc, (s, s + l.get()))))
            .map(|(pc, (s, e))| {
                (
                    pc,
                    program.shared_program_data.hints_collection.hints[s..e].to_vec(),
                )
            })
            .collect();
        assert_eq!(program_hints, hints);
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn new_program_with_identifiers() {
        let reference_manager = ReferenceManager {
            references: Vec::new(),
        };

        let builtins: Vec<BuiltinName> = Vec::new();

        let data: Vec<MaybeRelocatable> = vec![
            mayberelocatable!(5189976364521848832),
            mayberelocatable!(1000),
            mayberelocatable!(5189976364521848832),
            mayberelocatable!(2000),
            mayberelocatable!(5201798304953696256),
            mayberelocatable!(2345108766317314046),
        ];

        let mut identifiers: HashMap<String, Identifier> = HashMap::new();

        identifiers.insert(
            String::from("__main__.main"),
            Identifier {
                pc: Some(0),
                type_: Some(String::from("function")),
                value: None,
                full_name: None,
                members: None,
                cairo_type: None,
            },
        );

        identifiers.insert(
            String::from("__main__.main.SIZEOF_LOCALS"),
            Identifier {
                pc: None,
                type_: Some(String::from("const")),
                value: Some(Felt252::zero()),
                full_name: None,
                members: None,
                cairo_type: None,
            },
        );

        let program = Program::new(
            builtins.clone(),
            data.clone(),
            None,
            HashMap::new(),
            reference_manager,
            identifiers.clone(),
            Vec::new(),
            None,
        )
        .unwrap();

        assert_eq!(program.builtins, builtins);
        assert_eq!(program.shared_program_data.data, data);
        assert_eq!(program.shared_program_data.main, None);
        assert_eq!(program.shared_program_data.identifiers, identifiers);
        assert_eq!(
            program.constants,
            [("__main__.main.SIZEOF_LOCALS", Felt252::zero())]
                .into_iter()
                .map(|(key, value)| (key.to_string(), value))
                .collect::<HashMap<_, _>>(),
        );
    }

    #[test]
    fn extract_constants() {
        let mut identifiers: HashMap<String, Identifier> = HashMap::new();

        identifiers.insert(
            String::from("__main__.main"),
            Identifier {
                pc: Some(0),
                type_: Some(String::from("function")),
                value: None,
                full_name: None,
                members: None,
                cairo_type: None,
            },
        );

        identifiers.insert(
            String::from("__main__.main.SIZEOF_LOCALS"),
            Identifier {
                pc: None,
                type_: Some(String::from("const")),
                value: Some(Felt252::zero()),
                full_name: None,
                members: None,
                cairo_type: None,
            },
        );

        assert_eq!(
            Program::extract_constants(&identifiers).unwrap(),
            [("__main__.main.SIZEOF_LOCALS", Felt252::zero())]
                .into_iter()
                .map(|(key, value)| (key.to_string(), value))
                .collect::<HashMap<_, _>>(),
        );
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn get_prime() {
        let program = Program::default();
        assert_eq!(PRIME_STR, program.prime());
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn iter_builtins() {
        let reference_manager = ReferenceManager {
            references: Vec::new(),
        };

        let builtins: Vec<_> = vec![BuiltinName::range_check, BuiltinName::bitwise];
        let data: Vec<_> = vec![
            mayberelocatable!(5189976364521848832),
            mayberelocatable!(1000),
            mayberelocatable!(5189976364521848832),
            mayberelocatable!(2000),
            mayberelocatable!(5201798304953696256),
            mayberelocatable!(2345108766317314046),
        ];

        let program = Program::new(
            builtins.clone(),
            data,
            None,
            HashMap::new(),
            reference_manager,
            HashMap::new(),
            Vec::new(),
            None,
        )
        .unwrap();

        assert_eq!(
            program.iter_builtins().cloned().collect::<Vec<_>>(),
            builtins
        );

        assert_eq!(program.builtins_len(), 2);
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn iter_data() {
        let reference_manager = ReferenceManager {
            references: Vec::new(),
        };

        let builtins: Vec<BuiltinName> = Vec::new();
        let data: Vec<MaybeRelocatable> = vec![
            mayberelocatable!(5189976364521848832),
            mayberelocatable!(1000),
            mayberelocatable!(5189976364521848832),
            mayberelocatable!(2000),
            mayberelocatable!(5201798304953696256),
            mayberelocatable!(2345108766317314046),
        ];

        let program = Program::new(
            builtins,
            data.clone(),
            None,
            HashMap::new(),
            reference_manager,
            HashMap::new(),
            Vec::new(),
            None,
        )
        .unwrap();

        assert_eq!(program.iter_data().cloned().collect::<Vec<_>>(), data);
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn data_len() {
        let reference_manager = ReferenceManager {
            references: Vec::new(),
        };

        let builtins: Vec<BuiltinName> = Vec::new();
        let data: Vec<MaybeRelocatable> = vec![
            mayberelocatable!(5189976364521848832),
            mayberelocatable!(1000),
            mayberelocatable!(5189976364521848832),
            mayberelocatable!(2000),
            mayberelocatable!(5201798304953696256),
            mayberelocatable!(2345108766317314046),
        ];

        let program = Program::new(
            builtins,
            data.clone(),
            None,
            HashMap::new(),
            reference_manager,
            HashMap::new(),
            Vec::new(),
            None,
        )
        .unwrap();

        assert_eq!(program.data_len(), data.len());
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn get_identifier() {
        let reference_manager = ReferenceManager {
            references: Vec::new(),
        };

        let builtins: Vec<BuiltinName> = Vec::new();

        let data: Vec<MaybeRelocatable> = vec![
            mayberelocatable!(5189976364521848832),
            mayberelocatable!(1000),
            mayberelocatable!(5189976364521848832),
            mayberelocatable!(2000),
            mayberelocatable!(5201798304953696256),
            mayberelocatable!(2345108766317314046),
        ];

        let mut identifiers: HashMap<String, Identifier> = HashMap::new();

        identifiers.insert(
            String::from("__main__.main"),
            Identifier {
                pc: Some(0),
                type_: Some(String::from("function")),
                value: None,
                full_name: None,
                members: None,
                cairo_type: None,
            },
        );

        identifiers.insert(
            String::from("__main__.main.SIZEOF_LOCALS"),
            Identifier {
                pc: None,
                type_: Some(String::from("const")),
                value: Some(Felt252::zero()),
                full_name: None,
                members: None,
                cairo_type: None,
            },
        );

        let program = Program::new(
            builtins,
            data,
            None,
            HashMap::new(),
            reference_manager,
            identifiers.clone(),
            Vec::new(),
            None,
        )
        .unwrap();

        assert_eq!(
            program.get_identifier("__main__.main"),
            identifiers.get("__main__.main"),
        );
        assert_eq!(
            program.get_identifier("__main__.main.SIZEOF_LOCALS"),
            identifiers.get("__main__.main.SIZEOF_LOCALS"),
        );
        assert_eq!(
            program.get_identifier("missing"),
            identifiers.get("missing"),
        );
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn iter_identifiers() {
        let reference_manager = ReferenceManager {
            references: Vec::new(),
        };

        let builtins: Vec<BuiltinName> = Vec::new();

        let data: Vec<MaybeRelocatable> = vec![
            mayberelocatable!(5189976364521848832),
            mayberelocatable!(1000),
            mayberelocatable!(5189976364521848832),
            mayberelocatable!(2000),
            mayberelocatable!(5201798304953696256),
            mayberelocatable!(2345108766317314046),
        ];

        let mut identifiers: HashMap<String, Identifier> = HashMap::new();

        identifiers.insert(
            String::from("__main__.main"),
            Identifier {
                pc: Some(0),
                type_: Some(String::from("function")),
                value: None,
                full_name: None,
                members: None,
                cairo_type: None,
            },
        );

        identifiers.insert(
            String::from("__main__.main.SIZEOF_LOCALS"),
            Identifier {
                pc: None,
                type_: Some(String::from("const")),
                value: Some(Felt252::zero()),
                full_name: None,
                members: None,
                cairo_type: None,
            },
        );

        let program = Program::new(
            builtins,
            data,
            None,
            HashMap::new(),
            reference_manager,
            identifiers.clone(),
            Vec::new(),
            None,
        )
        .unwrap();

        let collected_identifiers: HashMap<_, _> = program
            .iter_identifiers()
            .map(|(cairo_type, identifier)| (cairo_type.to_string(), identifier.clone()))
            .collect();

        assert_eq!(collected_identifiers, identifiers);
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn new_program_with_invalid_identifiers() {
        let reference_manager = ReferenceManager {
            references: Vec::new(),
        };

        let builtins: Vec<BuiltinName> = Vec::new();

        let data: Vec<MaybeRelocatable> = vec![
            mayberelocatable!(5189976364521848832),
            mayberelocatable!(1000),
            mayberelocatable!(5189976364521848832),
            mayberelocatable!(2000),
            mayberelocatable!(5201798304953696256),
            mayberelocatable!(2345108766317314046),
        ];

        let mut identifiers: HashMap<String, Identifier> = HashMap::new();

        identifiers.insert(
            String::from("__main__.main"),
            Identifier {
                pc: Some(0),
                type_: Some(String::from("function")),
                value: None,
                full_name: None,
                members: None,
                cairo_type: None,
            },
        );

        identifiers.insert(
            String::from("__main__.main.SIZEOF_LOCALS"),
            Identifier {
                pc: None,
                type_: Some(String::from("const")),
                value: None,
                full_name: None,
                members: None,
                cairo_type: None,
            },
        );

        let program = Program::new(
            builtins,
            data,
            None,
            HashMap::new(),
            reference_manager,
            identifiers.clone(),
            Vec::new(),
            None,
        );

        assert!(program.is_err());
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn deserialize_program_test() {
        let program = Program::from_bytes(
            include_bytes!("../../../cairo_programs/manually_compiled/valid_program_a.json"),
            Some("main"),
        )
        .unwrap();

        let builtins: Vec<BuiltinName> = Vec::new();
        let data: Vec<MaybeRelocatable> = vec![
            mayberelocatable!(5189976364521848832),
            mayberelocatable!(1000),
            mayberelocatable!(5189976364521848832),
            mayberelocatable!(2000),
            mayberelocatable!(5201798304953696256),
            mayberelocatable!(2345108766317314046),
        ];

        let mut identifiers: HashMap<String, Identifier> = HashMap::new();

        identifiers.insert(
            String::from("__main__.main"),
            Identifier {
                pc: Some(0),
                type_: Some(String::from("function")),
                value: None,
                full_name: None,
                members: None,
                cairo_type: None,
            },
        );
        identifiers.insert(
            String::from("__main__.main.Args"),
            Identifier {
                pc: None,
                type_: Some(String::from("struct")),
                value: None,
                full_name: Some("__main__.main.Args".to_string()),
                members: Some(HashMap::new()),
                cairo_type: None,
            },
        );
        identifiers.insert(
            String::from("__main__.main.ImplicitArgs"),
            Identifier {
                pc: None,
                type_: Some(String::from("struct")),
                value: None,
                full_name: Some("__main__.main.ImplicitArgs".to_string()),
                members: Some(HashMap::new()),
                cairo_type: None,
            },
        );
        identifiers.insert(
            String::from("__main__.main.Return"),
            Identifier {
                pc: None,
                type_: Some(String::from("struct")),
                value: None,
                full_name: Some("__main__.main.Return".to_string()),
                members: Some(HashMap::new()),
                cairo_type: None,
            },
        );
        identifiers.insert(
            String::from("__main__.main.SIZEOF_LOCALS"),
            Identifier {
                pc: None,
                type_: Some(String::from("const")),
                value: Some(Felt252::zero()),
                full_name: None,
                members: None,
                cairo_type: None,
            },
        );

        assert_eq!(program.builtins, builtins);
        assert_eq!(program.shared_program_data.data, data);
        assert_eq!(program.shared_program_data.main, Some(0));
        assert_eq!(program.shared_program_data.identifiers, identifiers);
    }

    /// Deserialize a program without an entrypoint.
    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn deserialize_program_without_entrypoint_test() {
        let program = Program::from_bytes(
            include_bytes!("../../../cairo_programs/manually_compiled/valid_program_a.json"),
            None,
        )
        .unwrap();

        let builtins: Vec<BuiltinName> = Vec::new();

        let error_message_attributes: Vec<Attribute> = vec![Attribute {
            name: String::from("error_message"),
            start_pc: 379,
            end_pc: 381,
            value: String::from("SafeUint256: addition overflow"),
            flow_tracking_data: Some(FlowTrackingData {
                ap_tracking: ApTracking {
                    group: 14,
                    offset: 35,
                },
                reference_ids: HashMap::new(),
            }),
        }];

        let data: Vec<MaybeRelocatable> = vec![
            mayberelocatable!(5189976364521848832),
            mayberelocatable!(1000),
            mayberelocatable!(5189976364521848832),
            mayberelocatable!(2000),
            mayberelocatable!(5201798304953696256),
            mayberelocatable!(2345108766317314046),
        ];

        let mut identifiers: HashMap<String, Identifier> = HashMap::new();

        identifiers.insert(
            String::from("__main__.main"),
            Identifier {
                pc: Some(0),
                type_: Some(String::from("function")),
                value: None,
                full_name: None,
                members: None,
                cairo_type: None,
            },
        );
        identifiers.insert(
            String::from("__main__.main.Args"),
            Identifier {
                pc: None,
                type_: Some(String::from("struct")),
                value: None,
                full_name: Some("__main__.main.Args".to_string()),
                members: Some(HashMap::new()),
                cairo_type: None,
            },
        );
        identifiers.insert(
            String::from("__main__.main.ImplicitArgs"),
            Identifier {
                pc: None,
                type_: Some(String::from("struct")),
                value: None,
                full_name: Some("__main__.main.ImplicitArgs".to_string()),
                members: Some(HashMap::new()),
                cairo_type: None,
            },
        );
        identifiers.insert(
            String::from("__main__.main.Return"),
            Identifier {
                pc: None,
                type_: Some(String::from("struct")),
                value: None,
                full_name: Some("__main__.main.Return".to_string()),
                members: Some(HashMap::new()),
                cairo_type: None,
            },
        );
        identifiers.insert(
            String::from("__main__.main.SIZEOF_LOCALS"),
            Identifier {
                pc: None,
                type_: Some(String::from("const")),
                value: Some(Felt252::zero()),
                full_name: None,
                members: None,
                cairo_type: None,
            },
        );

        assert_eq!(program.builtins, builtins);
        assert_eq!(program.shared_program_data.data, data);
        assert_eq!(program.shared_program_data.main, None);
        assert_eq!(program.shared_program_data.identifiers, identifiers);
        assert_eq!(
            program.shared_program_data.error_message_attributes,
            error_message_attributes
        )
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn deserialize_program_constants_test() {
        let program = Program::from_bytes(
            include_bytes!(
                "../../../cairo_programs/manually_compiled/deserialize_constant_test.json"
            ),
            Some("main"),
        )
        .unwrap();

        let constants = [
            ("__main__.compare_abs_arrays.SIZEOF_LOCALS", Felt252::zero()),
            (
                "starkware.cairo.common.cairo_keccak.packed_keccak.ALL_ONES",
                felt_str!(
                    "3618502788666131106986593281521497120414687020801267626233049500247285301247"
                ),
            ),
            (
                "starkware.cairo.common.cairo_keccak.packed_keccak.BLOCK_SIZE",
                Felt252::new(3),
            ),
            (
                "starkware.cairo.common.alloc.alloc.SIZEOF_LOCALS",
                felt_str!(
                    "-3618502788666131213697322783095070105623107215331596699973092056135872020481"
                ),
            ),
            (
                "starkware.cairo.common.uint256.SHIFT",
                felt_str!("340282366920938463463374607431768211456"),
            ),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_string(), value))
        .collect::<HashMap<_, _>>();

        assert_eq!(program.constants, constants);
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn default_program() {
        let hints_collection = HintsCollection {
            hints: Vec::new(),
            hints_ranges: Vec::new(),
        };

        let shared_program_data = SharedProgramData {
            data: Vec::new(),
            hints_collection,
            main: None,
            start: None,
            end: None,
            error_message_attributes: Vec::new(),
            instruction_locations: None,
            identifiers: HashMap::new(),
            reference_manager: Program::get_reference_list(&ReferenceManager {
                references: Vec::new(),
            }),
        };
        let program = Program {
            shared_program_data: Arc::new(shared_program_data),
            constants: HashMap::new(),
            builtins: Vec::new(),
        };

        assert_eq!(program, Program::default());
    }

    #[test]
    fn get_stripped_program() {
        let program_content = include_bytes!("../../../cairo_programs/pedersen_test.json");
        let program = Program::from_bytes(program_content, Some("main")).unwrap();
        let stripped_program = program.get_stripped_program().unwrap();
        assert_eq!(stripped_program.builtins, program.builtins);
        assert_eq!(stripped_program.data, program.shared_program_data.data);
        assert_eq!(
            stripped_program.main,
            program.shared_program_data.main.unwrap()
        );
    }

    #[test]
    fn get_stripped_no_main() {
        let program_content =
            include_bytes!("../../../cairo_programs/proof_programs/fibonacci.json");
        let program = Program::from_bytes(program_content, None).unwrap();
        assert_matches!(
            program.get_stripped_program(),
            Err(ProgramError::StrippedProgramNoMain)
        );
    }
}
