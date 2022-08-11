use crate::{
    bigint,
    serde::deserialize_program::ApTracking,
    types::relocatable::MaybeRelocatable,
    vm::{
        errors::vm_errors::VirtualMachineError,
        hints::hint_utils::{get_integer_from_var_name, get_ptr_from_var_name},
        vm_core::VirtualMachine,
    },
};
use lazy_static::lazy_static;
use num_bigint::BigInt;
use num_traits::ToPrimitive;
use std::collections::HashMap;

lazy_static! {
    pub static ref BYTES_IN_WORD: BigInt = bigint!(8);
    pub static ref KECCAK_FULL_RATE_IN_BYTES: BigInt = bigint!(136);
}
const KECCAK_STATE_SIZE_FELTS: usize = 25;
const BLOCK_SIZE: usize = 3;

/*
Implements hint:
    %{
      segments.write_arg(ids.inputs, [ids.low % 2 ** 64, ids.low // 2 ** 64])
      segments.write_arg(ids.inputs + 2, [ids.high % 2 ** 64, ids.high // 2 ** 64])
    %}
*/
pub fn keccak_write_args(
    vm: &mut VirtualMachine,
    ids: &HashMap<String, BigInt>,
    hint_ap_tracking: Option<&ApTracking>,
) -> Result<(), VirtualMachineError> {
    let inputs_ptr = get_ptr_from_var_name(
        "inputs",
        ids,
        &vm.memory,
        &vm.references,
        &vm.run_context,
        hint_ap_tracking,
    )?;

    let low = get_integer_from_var_name(
        "low",
        ids,
        &vm.memory,
        &vm.references,
        &vm.run_context,
        hint_ap_tracking,
    )?;
    let high = get_integer_from_var_name(
        "high",
        ids,
        &vm.memory,
        &vm.references,
        &vm.run_context,
        hint_ap_tracking,
    )?;

    let low_args = [low & bigint!(u64::MAX), low >> 64];
    let high_args = [high & bigint!(u64::MAX), high >> 64];

    vm.segments
        .write_arg(
            &mut vm.memory,
            &inputs_ptr,
            &low_args.to_vec(),
            Some(&vm.prime),
        )
        .map_err(VirtualMachineError::MemoryError)?;

    vm.segments
        .write_arg(
            &mut vm.memory,
            &inputs_ptr.add(2)?,
            &high_args.to_vec(),
            Some(&vm.prime),
        )
        .map_err(VirtualMachineError::MemoryError)?;

    Ok(())
}

/*
Implements hint:
    Cairo code:
    if nondet %{ ids.n_bytes < ids.BYTES_IN_WORD %} != 0:

    Compiled code:
    memory[ap] = to_felt_or_relocatable(ids.n_bytes < ids.BYTES_IN_WORD)
*/
pub fn compare_bytes_in_word_nondet(
    vm: &mut VirtualMachine,
    ids: &HashMap<String, BigInt>,
    hint_ap_tracking: Option<&ApTracking>,
) -> Result<(), VirtualMachineError> {
    let n_bytes = get_integer_from_var_name(
        "n_bytes",
        ids,
        &vm.memory,
        &vm.references,
        &vm.run_context,
        hint_ap_tracking,
    )?;

    // This works fine, but it should be checked for a performance improvement.
    // One option is to try to convert n_bytes into usize, with failure to do so simply
    // making value be 0 (if it can't convert then it's either negative, which can't be in Cairo memory
    // or too big, which also means n_bytes > BYTES_IN_WORD). The other option is to exctract
    // bigint!(BYTES_INTO_WORD) into a lazy_static!
    let value = bigint!((n_bytes < &BYTES_IN_WORD) as usize);

    vm.memory
        .insert(&vm.run_context.ap, &MaybeRelocatable::from(value))
        .map_err(VirtualMachineError::MemoryError)
}

/*
Implements hint:
    Cairo code:
    if nondet %{ ids.n_bytes >= ids.KECCAK_FULL_RATE_IN_BYTES %} != 0:

    Compiled code:
    "memory[ap] = to_felt_or_relocatable(ids.n_bytes >= ids.KECCAK_FULL_RATE_IN_BYTES)"
*/
pub fn compare_keccak_full_rate_in_bytes_nondet(
    vm: &mut VirtualMachine,
    ids: &HashMap<String, BigInt>,
    hint_ap_tracking: Option<&ApTracking>,
) -> Result<(), VirtualMachineError> {
    let n_bytes = get_integer_from_var_name(
        "n_bytes",
        ids,
        &vm.memory,
        &vm.references,
        &vm.run_context,
        hint_ap_tracking,
    )?;

    let value = bigint!((n_bytes >= &KECCAK_FULL_RATE_IN_BYTES) as usize);

    vm.memory
        .insert(&vm.run_context.ap, &MaybeRelocatable::from(value))
        .map_err(VirtualMachineError::MemoryError)
}

/*
Implements hint:
    %{
        from starkware.cairo.common.cairo_keccak.keccak_utils import keccak_func
        _keccak_state_size_felts = int(ids.KECCAK_STATE_SIZE_FELTS)
        assert 0 <= _keccak_state_size_felts < 100

        output_values = keccak_func(memory.get_range(
            ids.keccak_ptr - _keccak_state_size_felts, _keccak_state_size_felts))
        segments.write_arg(ids.keccak_ptr, output_values)
    %}
*/
pub fn block_permutation(
    vm: &mut VirtualMachine,
    ids: &HashMap<String, BigInt>,
    hint_ap_tracking: Option<&ApTracking>,
) -> Result<(), VirtualMachineError> {
    // these checks may not make sense now, but will when constants are
    // deserialized from the compiled JSON programs.
    if KECCAK_STATE_SIZE_FELTS >= 100 {
        return Err(VirtualMachineError::InvalidKeccakStateSizeFelts(
            KECCAK_STATE_SIZE_FELTS,
        ));
    }

    let keccak_ptr = get_ptr_from_var_name(
        "keccak_ptr",
        ids,
        &vm.memory,
        &vm.references,
        &vm.run_context,
        hint_ap_tracking,
    )?;

    let values = vm
        .memory
        .get_range(
            &MaybeRelocatable::RelocatableValue(keccak_ptr.sub(KECCAK_STATE_SIZE_FELTS)?),
            KECCAK_STATE_SIZE_FELTS,
        )
        .map_err(VirtualMachineError::MemoryError)?;

    let mut u64_values = maybe_reloc_vec_to_u64_array(&values)?;

    // this function of the keccak crate is the one used instead of keccak_func from
    // keccak_utils.py
    keccak::f1600(&mut u64_values);

    let bigint_values = u64_array_to_bigint_vec(&u64_values);

    vm.segments
        .write_arg(&mut vm.memory, &keccak_ptr, &bigint_values, Some(&vm.prime))
        .map_err(VirtualMachineError::MemoryError)?;

    Ok(())
}

/* Implements hint:
    %{
        # Add dummy pairs of input and output.
        _keccak_state_size_felts = int(ids.KECCAK_STATE_SIZE_FELTS)
        _block_size = int(ids.BLOCK_SIZE)
        assert 0 <= _keccak_state_size_felts < 100
        assert 0 <= _block_size < 10
        inp = [0] * _keccak_state_size_felts
        padding = (inp + keccak_func(inp)) * _block_size
        segments.write_arg(ids.keccak_ptr_end, padding)
    %}
*/
pub fn cairo_keccak_finalize(
    vm: &mut VirtualMachine,
    ids: &HashMap<String, BigInt>,
    hint_ap_tracking: Option<&ApTracking>,
) -> Result<(), VirtualMachineError> {
    // these checks may not make sense now, but will when constants are
    // deserialized from the compiled JSON programs.
    if KECCAK_STATE_SIZE_FELTS >= 100 {
        return Err(VirtualMachineError::InvalidKeccakStateSizeFelts(
            KECCAK_STATE_SIZE_FELTS,
        ));
    }

    if BLOCK_SIZE >= 10 {
        return Err(VirtualMachineError::InvalidBlockSize(BLOCK_SIZE));
    }

    let mut inp = [0u64; KECCAK_STATE_SIZE_FELTS];
    keccak::f1600(&mut inp);

    let mut padding = vec![bigint!(0_u64); KECCAK_STATE_SIZE_FELTS];
    padding.extend(u64_array_to_bigint_vec(&inp));

    let base_padding = padding.clone();

    for _ in 0..(BLOCK_SIZE - 1) {
        padding.extend_from_slice(base_padding.as_slice());
    }

    let keccak_ptr_end = get_ptr_from_var_name(
        "keccak_ptr_end",
        ids,
        &vm.memory,
        &vm.references,
        &vm.run_context,
        hint_ap_tracking,
    )?;

    vm.segments
        .write_arg(&mut vm.memory, &keccak_ptr_end, &padding, Some(&vm.prime))
        .map_err(VirtualMachineError::MemoryError)?;

    Ok(())
}

// Helper function to transform a vector of MaybeRelocatables into a vector
// of u64. Raises error if there are None's or if MaybeRelocatables are not Bigints.
fn maybe_reloc_vec_to_u64_array(
    vec: &[Option<&MaybeRelocatable>],
) -> Result<[u64; KECCAK_STATE_SIZE_FELTS], VirtualMachineError> {
    let array: [u64; KECCAK_STATE_SIZE_FELTS] = vec
        .iter()
        .map(|n| {
            if let Some(MaybeRelocatable::Int(num)) = n {
                num.to_u64().ok_or(VirtualMachineError::BigintToU64Fail)
            } else {
                Err(VirtualMachineError::ExpectedIntAtRange(n.cloned()))
            }
        })
        .collect::<Result<Vec<u64>, VirtualMachineError>>()?
        .try_into()
        .map_err(|_| VirtualMachineError::BigintToU32Fail)?;

    Ok(array)
}

fn u64_array_to_bigint_vec(array: &[u64; KECCAK_STATE_SIZE_FELTS]) -> Vec<BigInt> {
    array.iter().map(|n| bigint!(*n)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::test_utils::*;
    use crate::vm::errors::memory_errors::MemoryError;
    use crate::vm::hints::execute_hint::BuiltinHintExecutor;
    use crate::vm::hints::execute_hint::HintReference;
    use crate::vm::runners::builtin_runner::RangeCheckBuiltinRunner;
    use crate::vm::vm_memory::memory::Memory;
    use num_bigint::{BigInt, Sign};
    static HINT_EXECUTOR: BuiltinHintExecutor = BuiltinHintExecutor {};

    #[test]
    fn keccak_write_args_valid_test() {
        let hint_code = "segments.write_arg(ids.inputs, [ids.low % 2 ** 64, ids.low // 2 ** 64])\nsegments.write_arg(ids.inputs + 2, [ids.high % 2 ** 64, ids.high // 2 ** 64])";
        let mut vm = vm_with_range_check!();

        for _ in 0..1 {
            vm.segments.add(&mut vm.memory, None);
        }

        vm.memory = memory![
            ((0, 0), 233),
            ((0, 1), 351),
            ((0, 2), (1, 0)),
            ((1, 4), 5_i32)
        ];

        //Initialize fp
        vm.run_context.fp = MaybeRelocatable::from((0, 3));

        //Create ids
        let ids = ids!["low", "high", "inputs"];
        vm.references = references!(3);

        assert_eq!(
            vm.hint_executor
                .execute_hint(&mut vm, hint_code, &ids, &ApTracking::new()),
            Ok(())
        );
    }

    #[test]
    fn keccak_write_args_write_error() {
        let hint_code = "segments.write_arg(ids.inputs, [ids.low % 2 ** 64, ids.low // 2 ** 64])\nsegments.write_arg(ids.inputs + 2, [ids.high % 2 ** 64, ids.high // 2 ** 64])";
        let mut vm = vm_with_range_check!();

        for _ in 0..1 {
            vm.segments.add(&mut vm.memory, None);
        }

        vm.memory = memory![((0, 0), 233), ((0, 1), 351), ((0, 2), (1, 0))];

        //Initialize fp
        vm.run_context.fp = MaybeRelocatable::from((0, 3));

        //Create ids
        let ids = ids!["low", "high", "inputs"];
        vm.references = references!(3);

        let error = vm
            .hint_executor
            .execute_hint(&mut vm, hint_code, &ids, &ApTracking::new());

        assert!(matches!(error, Err(VirtualMachineError::MemoryError(_))));
    }

    #[test]
    fn compare_bytes_in_word_nondet_valid() {
        let hint_code =
            "memory[ap] = to_felt_or_relocatable(ids.n_bytes >= ids.KECCAK_FULL_RATE_IN_BYTES)";
        let mut vm = vm_with_range_check!();

        vm.segments.add(&mut vm.memory, None);
        vm.memory = memory![((0, 0), 24)];

        vm.run_context.fp = MaybeRelocatable::from((0, 1));
        vm.run_context.ap = MaybeRelocatable::from((0, 1));

        let ids = ids!["n_bytes"];
        vm.references = references!(1);

        assert_eq!(
            vm.hint_executor
                .execute_hint(&mut vm, hint_code, &ids, &ApTracking::new()),
            Ok(())
        );
    }

    #[test]
    fn compare_keccak_full_rate_in_bytes_nondet_valid() {
        let hint_code =
            "memory[ap] = to_felt_or_relocatable(ids.n_bytes >= ids.KECCAK_FULL_RATE_IN_BYTES)";

        let mut vm = vm_with_range_check!();

        vm.segments.add(&mut vm.memory, None);
        vm.memory = memory![((0, 0), 24)];

        vm.run_context.fp = MaybeRelocatable::from((0, 1));
        vm.run_context.ap = MaybeRelocatable::from((0, 1));

        let ids = ids!["n_bytes"];
        vm.references = references!(1);

        assert_eq!(
            vm.hint_executor
                .execute_hint(&mut vm, hint_code, &ids, &ApTracking::new()),
            Ok(())
        );
    }

    #[test]
    fn block_permutation_valid_test() {
        let hint_code =
            "memory[ap] = to_felt_or_relocatable(ids.n_bytes >= ids.KECCAK_FULL_RATE_IN_BYTES)";
        let mut vm = vm_with_range_check!();

        vm.segments.add(&mut vm.memory, None);
        vm.memory = memory![((0, 0), 24)];

        vm.run_context.fp = MaybeRelocatable::from((0, 1));
        vm.run_context.ap = MaybeRelocatable::from((0, 1));

        let ids = ids!["n_bytes"];
        vm.references = references!(1);

        assert_eq!(
            vm.hint_executor
                .execute_hint(&mut vm, hint_code, &ids, &ApTracking::new()),
            Ok(())
        );
    }
}