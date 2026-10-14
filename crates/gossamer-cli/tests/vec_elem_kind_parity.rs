//! The element-kind tags codegen writes into a vector's header are the ones
//! the runtime reads them as.

use gossamer_runtime::c_abi::vec::vec_elem_kind as runtime;
use gossamer_types::vec_elem_kind as codegen;

#[test]
fn codegen_element_kinds_are_the_runtime_element_kinds() {
    let pairs = [
        (codegen::PRIMITIVE, runtime::PRIMITIVE),
        (codegen::STRING, runtime::STRING),
        (codegen::VEC, runtime::VEC),
        (codegen::MAP, runtime::MAP),
        (codegen::ERROR, runtime::ERROR),
        (codegen::AGGR_FLAT, runtime::AGGR_FLAT),
        (codegen::JSON, runtime::JSON),
        (codegen::SIGNED, runtime::SIGNED),
        (codegen::F32, runtime::F32),
    ];
    for (written, read) in pairs {
        assert_eq!(written, read);
    }
}
