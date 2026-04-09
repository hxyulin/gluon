extern crate proc_macro;
use proc_macro::TokenStream;

#[proc_macro_derive(Hello)]
pub fn hello(_: TokenStream) -> TokenStream {
    "const HELLO: &str = \"hello\";".parse().unwrap()
}
