//@ known-bug: #121574
#![feature(generic_const_exprs)]
pub struct DimName<const N: usize> {}
impl<const Z: usize> X<Z> {
    pub fn y<'a, U: 'a>(&'a self) -> impl Iterator<Item = impl Iterator<Item = [u8; Z]> + '_> {
        "0".as_bytes(move |_| (0..1).map(move |_| loop {}))
    }
}
