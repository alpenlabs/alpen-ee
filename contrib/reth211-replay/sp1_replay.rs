use std::{env, fs, sync::Arc};
use sp1_core_executor::{MinimalExecutorEnum, Program};

fn main() {
    let args: Vec<_> = env::args_os().skip(1).collect();
    assert_eq!(args.len(), 3, "usage: local-sp1-replay ELF INPUT EXPECTED-PUBLIC-VALUES");
    let elf = fs::read(&args[0]).unwrap();
    let input = fs::read(&args[1]).unwrap();
    let public = fs::read(&args[2]).unwrap();
    let program = Arc::new(Program::from(&elf).expect("valid SP1 ELF"));
    let mut executor = MinimalExecutorEnum::new(program, false, None);
    executor.with_input(&input);
    while executor.execute_chunk().is_some() {}
    assert_eq!(executor.exit_code(), 0, "SP1 guest did not halt successfully");
    assert_eq!(executor.public_values_stream().as_slice(), &public);
    println!("SP1 execution matched all {} canonical public-output bytes; cycles={}", public.len(), executor.global_clk());
}
