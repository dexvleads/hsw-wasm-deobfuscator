use crate::transformations::memory::visitors::{LoadMemoryFuncMapper, StoreMemoryFuncMapper};
use crate::transformations::memory::MemEncFuncType;
use crate::transformations::Transformer;
use std::collections::{HashMap, VecDeque};
use walrus::ir::{
    BinaryOp, Block, ExtendedLoad, IfElse, Instr, Load, LoadKind, Loop, MemArg, Store, StoreKind,
    Value,
};
use walrus::{
    ConstExpr, DataKind, FunctionId, FunctionKind, InstrLocId, Module, ValType,
};

pub struct MemoryTransformer {}

impl Transformer for MemoryTransformer {
    fn transform(&mut self, module: &mut Module) {
        let mapped_load_functions = self.map_load_functions(module);
        let mapped_store_functions = self.map_store_functions(module);

        let xor_table = self.get_xor_table(module);

        // println!("Load functions: {:?}", mapped_load_functions);
        // println!("Store functions: {:?}", mapped_store_functions);

        let wasm_data = module.data.iter().nth(1).unwrap();
        let data_start = match &wasm_data.kind {
            DataKind::Active { offset, .. } => match *offset {
                ConstExpr::Value(v) => match v {
                    Value::I32(i) => i,
                    _ => panic!(),
                },
                _ => panic!(),
            },
            _ => panic!(),
        } as usize;
        
        let start_pos = data_start-((data_start/320)<<3)-320-23; // 23 is hardcoded btw
        let mut new_data = Vec::<u8>::with_capacity(wasm_data.value.len());

        for (i, _) in wasm_data.value.iter().enumerate() {
            let pos = start_pos + i;

            let res = self.read_byte(data_start, &wasm_data.value, &xor_table, pos);
            if let Some(res) = res {
                new_data.push(res);
            } else {
                break;
            }
        }
        
        // println!("Decrypted {} bytes", new_data.len());

        while new_data.len() < wasm_data.value.len() {
            new_data.push(0);
        }

        // println!("Prev data len: {}", wasm_data.value.len());
        // println!("New data len: {}", new_data.len());

        // replace data with our new decrypted data
        {
            let mem_id = module.get_memory_id().unwrap();
            let data = module.data.get_mut(wasm_data.id());
            
            data.value = new_data;
            data.kind = DataKind::Active {
                memory: mem_id,
                offset: ConstExpr::Value(Value::I32(start_pos as i32)),
            }
        }
        
        self.revert_memory_loads(module, &mapped_load_functions);
        self.revert_memory_stores(module, &mapped_store_functions);
        self.rewrite_loads(module, &mapped_load_functions);
        self.rewrite_stores(module, &mapped_store_functions);
    }
}

impl MemoryTransformer {
    // Finds every function that could possibly match with mem load funcs.
    // 2 params + 1 result + exported
    fn find_mem_load_functions(&self, module: &Module) -> Vec<FunctionId> {
        let mut functions = Vec::new();

        'a: for function in module.funcs.iter() {
            let function_id = function.id();
            if let FunctionKind::Local(local) = &function.kind {
                let t = module.types.get(local.ty());
                if t.params().len() != 2 {
                    // idx, offset
                    continue 'a;
                }

                for param in t.params() {
                    if !matches!(param, ValType::I32) {
                        continue 'a;
                    }
                }

                if t.results().len() != 1 {
                    continue 'a;
                }

                if !module.exports.get_exported_func(function_id).is_some() {
                    continue 'a;
                }

                functions.push(function_id);
            }
        }

        functions
    }

    fn map_load_functions(&self, module: &mut Module) -> HashMap<FunctionId, MemEncFuncType> {
        let mut mapped_load_functions = HashMap::new();
        let load_functions = self.find_mem_load_functions(module);

        for id in load_functions.into_iter() {
            let func = module.funcs.get(id); // ALWAYS local
            let local = func.kind.unwrap_local();
            let t = module.types.get(local.ty());

            match t.results()[0] {
                ValType::I32 => {
                    let mut visitor = LoadMemoryFuncMapper::default();
                    if let Some(load_type) = visitor.map(&local) {
                        mapped_load_functions.insert(id, load_type);
                    }
                }
                ValType::F32 => {
                    mapped_load_functions.insert(id, MemEncFuncType::Float32);
                }
                ValType::F64 => {
                    mapped_load_functions.insert(id, MemEncFuncType::Float64);
                }
                ValType::I64 => {
                    mapped_load_functions.insert(id, MemEncFuncType::Signed64);
                }
                _ => unreachable!(), // what the flip
            };
        }

        mapped_load_functions
    }

    fn revert_memory_loads(
        &self,
        module: &mut Module,
        functions: &HashMap<FunctionId, MemEncFuncType>,
    ) {
        let memory_id = module.memories.iter().next().unwrap().id();

        module.funcs.iter_local_mut().for_each(|(_, f)| {
            let mut stack = VecDeque::new();
            stack.push_front(f.entry_block());
            while let Some(block_id) = stack.pop_back() {
                let block = f.block_mut(block_id);

                let mut replacements = Vec::<(usize, (Instr, InstrLocId))>::new();

                for (idx, (instr, instr_id)) in block.instrs.iter().enumerate() {
                    match instr {
                        Instr::Block(Block { seq }) | Instr::Loop(Loop { seq }) => {
                            stack.push_front(*seq)
                        }
                        Instr::IfElse(IfElse {
                            consequent,
                            alternative,
                        }) => {
                            stack.push_front(*consequent);
                            stack.push_front(*alternative);
                        }
                        Instr::Const(c) => match c.value {
                            Value::I32(i) => {
                                let next_instruction = block.instrs.get(idx + 1);
                                if let Some((next_instruction, _)) = next_instruction {
                                    if let Instr::Call(call) = next_instruction {
                                        if let Some(func_type) = functions.get(&call.func) {
                                            match func_type {
                                                MemEncFuncType::Signed64 => {
                                                    replacements.push((
                                                        idx,
                                                        (
                                                            Instr::Load(Load {
                                                                memory: memory_id,
                                                                kind: LoadKind::I64 {
                                                                    atomic: false,
                                                                },
                                                                arg: MemArg {
                                                                    align: 8,
                                                                    offset: i as u32,
                                                                },
                                                            }),
                                                            *instr_id,
                                                        ),
                                                    ));
                                                }
                                                MemEncFuncType::Signed32 => {
                                                    replacements.push((
                                                        idx,
                                                        (
                                                            Instr::Load(Load {
                                                                memory: memory_id,
                                                                kind: LoadKind::I32 {
                                                                    atomic: false,
                                                                },
                                                                arg: MemArg {
                                                                    align: 4,
                                                                    offset: i as u32,
                                                                },
                                                            }),
                                                            *instr_id,
                                                        ),
                                                    ));
                                                }
                                                MemEncFuncType::Signed16 => {
                                                    replacements.push((
                                                        idx,
                                                        (
                                                            Instr::Load(Load {
                                                                memory: memory_id,
                                                                kind: LoadKind::I32_16 {
                                                                    kind: ExtendedLoad::SignExtend,
                                                                },
                                                                arg: MemArg {
                                                                    align: 2,
                                                                    offset: i as u32,
                                                                },
                                                            }),
                                                            *instr_id,
                                                        ),
                                                    ));
                                                }
                                                MemEncFuncType::Unsigned16 => {
                                                    replacements.push((
                                                        idx,
                                                        (
                                                            Instr::Load(Load {
                                                                memory: memory_id,
                                                                kind: LoadKind::I32_16 {
                                                                    kind: ExtendedLoad::ZeroExtend,
                                                                },
                                                                arg: MemArg {
                                                                    align: 2,
                                                                    offset: i as u32,
                                                                },
                                                            }),
                                                            *instr_id,
                                                        ),
                                                    ));
                                                }
                                                MemEncFuncType::Unsigned8 => {
                                                    replacements.push((
                                                        idx,
                                                        (
                                                            Instr::Load(Load {
                                                                memory: memory_id,
                                                                kind: LoadKind::I32_8 {
                                                                    kind: ExtendedLoad::ZeroExtend,
                                                                },
                                                                arg: MemArg {
                                                                    align: 1,
                                                                    offset: i as u32,
                                                                },
                                                            }),
                                                            *instr_id,
                                                        ),
                                                    ));
                                                }
                                                MemEncFuncType::Signed8 => {
                                                    replacements.push((
                                                        idx,
                                                        (
                                                            Instr::Load(Load {
                                                                memory: memory_id,
                                                                kind: LoadKind::I32_8 {
                                                                    kind: ExtendedLoad::SignExtend,
                                                                },
                                                                arg: MemArg {
                                                                    align: 1,
                                                                    offset: i as u32,
                                                                },
                                                            }),
                                                            *instr_id,
                                                        ),
                                                    ));
                                                }
                                                MemEncFuncType::Float32 => {
                                                    replacements.push((
                                                        idx,
                                                        (
                                                            Instr::Load(Load {
                                                                memory: memory_id,
                                                                kind: LoadKind::F32,
                                                                arg: MemArg {
                                                                    align: 4,
                                                                    offset: i as u32,
                                                                },
                                                            }),
                                                            *instr_id,
                                                        ),
                                                    ));
                                                }
                                                MemEncFuncType::Float64 => {
                                                    replacements.push((
                                                        idx,
                                                        (
                                                            Instr::Load(Load {
                                                                memory: memory_id,
                                                                kind: LoadKind::F64,
                                                                arg: MemArg {
                                                                    align: 8,
                                                                    offset: i as u32,
                                                                },
                                                            }),
                                                            *instr_id,
                                                        ),
                                                    ));
                                                }
                                                _ => unreachable!(),
                                            }
                                        }
                                    }
                                }
                            }
                            _ => {}
                        },
                        _ => {}
                    };
                }

                replacements.reverse();
                for (idx, (instr, seq)) in replacements {
                    block.instrs[idx] = (instr, seq);
                    block.instrs.remove(idx + 1);
                }
            }
        });
    }

    fn revert_memory_stores(
        &self,
        module: &mut Module,
        functions: &HashMap<FunctionId, MemEncFuncType>,
    ) {
        let memory_id = module.memories.iter().next().unwrap().id();

        module.funcs.iter_local_mut().for_each(|(_, f)| {
            let mut stack = VecDeque::new();
            stack.push_front(f.entry_block());
            while let Some(block_id) = stack.pop_back() {
                let block = f.block_mut(block_id);

                let mut replacements = Vec::<(usize, (Instr, InstrLocId))>::new();

                for (idx, (instr, instr_id)) in block.instrs.iter().enumerate() {
                    match instr {
                        Instr::Block(Block { seq }) | Instr::Loop(Loop { seq }) => {
                            stack.push_front(*seq)
                        }
                        Instr::IfElse(IfElse {
                            consequent,
                            alternative,
                        }) => {
                            stack.push_front(*consequent);
                            stack.push_front(*alternative);
                        }
                        Instr::Const(c) => match c.value {
                            Value::I32(i) => {
                                let next_instruction = block.instrs.get(idx + 1);
                                if let Some((next_instruction, _)) = next_instruction {
                                    if let Instr::Call(call) = next_instruction {
                                        if let Some(func_type) = functions.get(&call.func) {
                                            match func_type {
                                                MemEncFuncType::Signed64 => {
                                                    replacements.push((
                                                        idx,
                                                        (
                                                            Instr::Store(Store {
                                                                memory: memory_id,
                                                                kind: StoreKind::I64 {
                                                                    atomic: false,
                                                                },
                                                                arg: MemArg {
                                                                    align: 8,
                                                                    offset: i as u32,
                                                                },
                                                            }),
                                                            *instr_id,
                                                        ),
                                                    ));
                                                }
                                                MemEncFuncType::Signed32 => {
                                                    replacements.push((
                                                        idx,
                                                        (
                                                            Instr::Store(Store {
                                                                memory: memory_id,
                                                                kind: StoreKind::I32 {
                                                                    atomic: false,
                                                                },
                                                                arg: MemArg {
                                                                    align: 4,
                                                                    offset: i as u32,
                                                                },
                                                            }),
                                                            *instr_id,
                                                        ),
                                                    ));
                                                }
                                                MemEncFuncType::Signed16
                                                | MemEncFuncType::Unsigned16 => {
                                                    replacements.push((
                                                        idx,
                                                        (
                                                            Instr::Store(Store {
                                                                memory: memory_id,
                                                                kind: StoreKind::I32_16 {
                                                                    atomic: false,
                                                                },
                                                                arg: MemArg {
                                                                    align: 2,
                                                                    offset: i as u32,
                                                                },
                                                            }),
                                                            *instr_id,
                                                        ),
                                                    ));
                                                }
                                                MemEncFuncType::Signed8
                                                | MemEncFuncType::Unsigned8 => {
                                                    replacements.push((
                                                        idx,
                                                        (
                                                            Instr::Store(Store {
                                                                memory: memory_id,
                                                                kind: StoreKind::I32_8 {
                                                                    atomic: false,
                                                                },
                                                                arg: MemArg {
                                                                    align: 1,
                                                                    offset: i as u32,
                                                                },
                                                            }),
                                                            *instr_id,
                                                        ),
                                                    ));
                                                }
                                                MemEncFuncType::Float32 => {
                                                    replacements.push((
                                                        idx,
                                                        (
                                                            Instr::Store(Store {
                                                                memory: memory_id,
                                                                kind: StoreKind::F32,
                                                                arg: MemArg {
                                                                    align: 4,
                                                                    offset: i as u32,
                                                                },
                                                            }),
                                                            *instr_id,
                                                        ),
                                                    ));
                                                }
                                                MemEncFuncType::Float64 => {
                                                    replacements.push((
                                                        idx,
                                                        (
                                                            Instr::Store(Store {
                                                                memory: memory_id,
                                                                kind: StoreKind::F64,
                                                                arg: MemArg {
                                                                    align: 8,
                                                                    offset: i as u32,
                                                                },
                                                            }),
                                                            *instr_id,
                                                        ),
                                                    ));
                                                }
                                                _ => unreachable!(),
                                            }
                                        }
                                    }
                                }
                            }
                            _ => {}
                        },
                        _ => {}
                    };
                }

                replacements.reverse(); // hell yeah
                for (idx, (instr, seq)) in replacements {
                    block.instrs[idx] = (instr, seq);
                    block.instrs.remove(idx + 1);
                }
            }
        });
    }

    fn find_mem_store_functions(&self, module: &Module) -> Vec<FunctionId> {
        let mut functions = Vec::new();

        'a: for function in module.funcs.iter() {
            let function_id = function.id();
            if let FunctionKind::Local(local) = &function.kind {
                let t = module.types.get(local.ty());
                if t.params().len() != 3 {
                    // idx, offset
                    continue 'a;
                }

                for (i, param) in t.params().iter().enumerate() {
                    if !matches!(param, ValType::I32) && i != 1 {
                        continue 'a;
                    }
                }

                if t.results().len() != 0 {
                    continue 'a;
                }

                if !module.exports.get_exported_func(function_id).is_some() {
                    continue 'a;
                }

                functions.push(function_id);
            }
        }

        functions
    }

    fn map_store_functions(&self, module: &mut Module) -> HashMap<FunctionId, MemEncFuncType> {
        let mut mapped_store_functions = HashMap::new();
        let store_functions = self.find_mem_store_functions(module);

        for id in store_functions.into_iter() {
            let func = module.funcs.get(id); // ALWAYS local
            let local = func.kind.unwrap_local();
            let t = module.types.get(local.ty());

            match &t.params()[1] {
                ValType::I64 => {
                    mapped_store_functions.insert(id, MemEncFuncType::Signed64);
                }
                ValType::F32 => {
                    mapped_store_functions.insert(id, MemEncFuncType::Float32);
                }
                ValType::F64 => {
                    mapped_store_functions.insert(id, MemEncFuncType::Float64);
                }
                _ => {
                    let mut visitor = StoreMemoryFuncMapper::default();
                    if let Some(load_type) = visitor.map(&local) {
                        mapped_store_functions.insert(id, load_type);
                    }
                }
            }
        }

        mapped_store_functions
    }

    fn read_byte(
        &self,
        data_start: usize,
        data: &Vec<u8>,
        xor_table: &Vec<u8>,
        pos: usize,
    ) -> Option<u8> {
        let var0 = pos;
        let var1 = var0 / 320;
        let var2 = (var1 << 3) + var0 + 1032;

        let v = xor_table[var0 % 96];
        let result = if *data.get((var1 * 328 + 1024) - data_start)? > 0 {
            data[var2 - data_start]
        } else {
            v
        };

        Some(result ^ v)
    }

    fn rewrite_loads(&self, module: &mut Module, functions: &HashMap<FunctionId, MemEncFuncType>) {
        let memory_id = module.memories.iter().next().unwrap().id();

        for (id, func_type) in functions.into_iter() {
            let func = module.funcs.get_mut(*id).kind.unwrap_local_mut();


            let idx_local = *func.args.get(0).unwrap();
            let offset_local = *func.args.get(1).unwrap();

            match func_type {
                MemEncFuncType::Unsigned8 => {
                    func.builder_mut()
                        .func_body()
                        .local_get_at(0, idx_local)
                        .local_get_at(1, offset_local)
                        .binop_at(2, BinaryOp::I32Add)
                        .load_at(3, memory_id, LoadKind::I32_8 { kind: ExtendedLoad::ZeroExtend }, MemArg { align: 1, offset: 0 })
                        .return_at(4);
                }
                MemEncFuncType::Signed8 => {
                    func.builder_mut()
                        .func_body()
                        .local_get_at(0, idx_local)
                        .local_get_at(1, offset_local)
                        .binop_at(2, BinaryOp::I32Add)
                        .load_at(3, memory_id, LoadKind::I32_8 { kind: ExtendedLoad::SignExtend }, MemArg { align: 1, offset: 0 })
                        .return_at(4);
                }
                MemEncFuncType::Unsigned16 => {
                    func.builder_mut()
                        .func_body()
                        .local_get_at(0, idx_local)
                        .local_get_at(1, offset_local)
                        .binop_at(2, BinaryOp::I32Add)
                        .load_at(3, memory_id, LoadKind::I32_16 { kind: ExtendedLoad::ZeroExtend }, MemArg { align: 2, offset: 0 })
                        .return_at(4);
                }
                MemEncFuncType::Signed16 => {
                    func.builder_mut()
                        .func_body()
                        .local_get_at(0, idx_local)
                        .local_get_at(1, offset_local)
                        .binop_at(2, BinaryOp::I32Add)
                        .load_at(3, memory_id, LoadKind::I32_16 { kind: ExtendedLoad::SignExtend }, MemArg { align: 2, offset: 0 })
                        .return_at(4);
                }
                MemEncFuncType::Signed32 => {
                    func.builder_mut()
                        .func_body()
                        .local_get_at(0, idx_local)
                        .local_get_at(1, offset_local)
                        .binop_at(2, BinaryOp::I32Add)
                        .load_at(3, memory_id, LoadKind::I32 { atomic: false }, MemArg { align: 4, offset: 0 })
                        .return_at(4);
                }
                MemEncFuncType::Signed64 => {
                    func.builder_mut()
                        .func_body()
                        .local_get_at(0, idx_local)
                        .local_get_at(1, offset_local)
                        .binop_at(2, BinaryOp::I32Add)
                        .load_at(3, memory_id, LoadKind::I64 { atomic: false }, MemArg { align: 8, offset: 0 })
                        .return_at(4);
                }
                MemEncFuncType::Float32 => {
                    func.builder_mut()
                        .func_body()
                        .local_get_at(0, idx_local)
                        .local_get_at(1, offset_local)
                        .binop_at(2, BinaryOp::I32Add)
                        .load_at(3, memory_id, LoadKind::F32, MemArg { align: 4, offset: 0 })
                        .return_at(4);
                }
                MemEncFuncType::Float64 => {
                    func.builder_mut()
                        .func_body()
                        .local_get_at(0, idx_local)
                        .local_get_at(1, offset_local)
                        .binop_at(2, BinaryOp::I32Add)
                        .load_at(3, memory_id, LoadKind::F64, MemArg { align: 8, offset: 0 })
                        .return_at(4);
                }
            }
        }
    }

    fn rewrite_stores(&self, module: &mut Module, functions: &HashMap<FunctionId, MemEncFuncType>) {
        let memory_id = module.memories.iter().next().unwrap().id();

        for (id, func_type) in functions.into_iter() {
            let func = module.funcs.get_mut(*id).kind.unwrap_local_mut();

            let idx_local = *func.args.get(0).unwrap();
            let value_local = *func.args.get(1).unwrap();
            let offset_local = *func.args.get(2).unwrap();

            match func_type {
                MemEncFuncType::Unsigned8 | MemEncFuncType::Signed8 => {
                    func.builder_mut()
                        .func_body()
                        .local_get_at(0, idx_local)
                        .local_get_at(1, offset_local)
                        .binop_at(2, BinaryOp::I32Add)
                        .local_get_at(3, value_local)
                        .store_at(4, memory_id, StoreKind::I32_8 { atomic: false }, MemArg { align: 1, offset: 0 })
                        .return_at(5);
                }
                MemEncFuncType::Unsigned16 | MemEncFuncType::Signed16 => {
                    func.builder_mut()
                        .func_body()
                        .local_get_at(0, idx_local)
                        .local_get_at(1, offset_local)
                        .binop_at(2, BinaryOp::I32Add)
                        .local_get_at(3, value_local)
                        .store_at(4, memory_id, StoreKind::I32_16 { atomic: false }, MemArg { align: 2, offset: 0 })
                        .return_at(5);
                }
                MemEncFuncType::Signed32 => {
                    func.builder_mut()
                        .func_body()
                        .local_get_at(0, idx_local)
                        .local_get_at(1, offset_local)
                        .binop_at(2, BinaryOp::I32Add)
                        .local_get_at(3, value_local)
                        .store_at(4, memory_id, StoreKind::I32 { atomic: false }, MemArg { align: 4, offset: 0 })
                        .return_at(5);
                }
                MemEncFuncType::Signed64 => {
                    func.builder_mut()
                        .func_body()
                        .local_get_at(0, idx_local)
                        .local_get_at(1, offset_local)
                        .binop_at(2, BinaryOp::I32Add)
                        .local_get_at(3, value_local)
                        .store_at(4, memory_id, StoreKind::I64 { atomic: false }, MemArg { align: 8, offset: 0 })
                        .return_at(5);
                }
                MemEncFuncType::Float32 => {
                    func.builder_mut()
                        .func_body()
                        .local_get_at(0, idx_local)
                        .local_get_at(1, offset_local)
                        .binop_at(2, BinaryOp::I32Add)
                        .local_get_at(3, value_local)
                        .store_at(4, memory_id, StoreKind::F32, MemArg { align: 4, offset: 0 })
                        .return_at(5);
                }
                MemEncFuncType::Float64 => {
                    func.builder_mut()
                        .func_body()
                        .local_get_at(0, idx_local)
                        .local_get_at(1, offset_local)
                        .binop_at(2, BinaryOp::I32Add)
                        .local_get_at(3, value_local)
                        .store_at(4, memory_id, StoreKind::F64, MemArg { align: 8, offset: 0 })
                        .return_at(5);
                }
            }
        }
    }

    // Retrieves xor table
    // Needs a function that loads a primitive from the memory (preferably unsigned byte)
    fn get_xor_table(&self, module: &Module) -> Vec<u8> {
        let mut xors = Vec::new();

        let data_segment = module.data.iter().next().unwrap();
        let data_start = match &data_segment.kind {
            DataKind::Active { offset, .. } => match *offset {
                ConstExpr::Value(v) => match v {
                    Value::I32(i) => i,
                    _ => panic!(),
                },
                _ => panic!(),
            },
            _ => panic!(),
        } as usize;

        let offset = 693; // TODO: handle this dynamically

        for i in 0..96 {
            // It seems like that the table always has 96 bytes
            xors.push(data_segment.value[offset + i - data_start]);
        }

        xors
    }
}

