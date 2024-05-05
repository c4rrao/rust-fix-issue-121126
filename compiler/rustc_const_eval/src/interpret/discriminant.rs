//! Functions for reading and writing discriminants of multi-variant layouts (enums and coroutines).

use rustc_middle::mir;
use rustc_middle::ty::layout::{LayoutOf, PrimitiveExt};
use rustc_middle::ty::{self, ScalarInt, Ty};
use rustc_target::abi::{self, TagEncoding};
use rustc_target::abi::{VariantIdx, Variants};

use super::{ImmTy, InterpCx, InterpResult, Machine, Readable, Scalar, Writeable};

impl<'mir, 'tcx: 'mir, M: Machine<'mir, 'tcx>> InterpCx<'mir, 'tcx, M> {
    /// Writes the discriminant of the given variant.
    ///
    /// If the variant is uninhabited, this is UB.
    #[instrument(skip(self), level = "trace")]
    pub fn write_discriminant(
        &mut self,
        variant_index: VariantIdx,
        dest: &impl Writeable<'tcx, M::Provenance>,
    ) -> InterpResult<'tcx> {
        // Layout computation excludes uninhabited variants from consideration
        // therefore there's no way to represent those variants in the given layout.
        // Essentially, uninhabited variants do not have a tag that corresponds to their
        // discriminant, so we cannot do anything here.
        // When evaluating we will always error before even getting here, but ConstProp 'executes'
        // dead code, so we cannot ICE here.
        if dest.layout().for_variant(self, variant_index).abi.is_uninhabited() {
            throw_ub!(UninhabitedEnumVariantWritten(variant_index))
        }

        match self.tag_for_variant(dest.layout().ty, variant_index)? {
            Some((tag, tag_field)) => {
                // No need to validate that the discriminant here because the
                // `TyAndLayout::for_variant()` call earlier already checks the
                // variant is valid.
                let tag_dest = self.project_field(dest, tag_field)?;
                self.write_scalar(tag, &tag_dest)
            }
            None => {
                // No need to write the tag here, because an untagged variant is
                // implicitly encoded. For `Niche`-optimized enums, this works by
                // simply by having a value that is outside the niche variants.
                // But what if the data stored here does not actually encode
                // this variant? That would be bad! So let's double-check...
                let actual_variant = self.read_discriminant(&dest.to_op(self)?)?;
                if actual_variant != variant_index {
                    throw_ub!(InvalidNichedEnumVariantWritten { enum_ty: dest.layout().ty });
                }
                Ok(())
            }
        }
    }

    /// Read discriminant, return the runtime value as well as the variant index.
    /// Can also legally be called on non-enums (e.g. through the discriminant_value intrinsic)!
    ///
    /// Will never return an uninhabited variant.
    #[instrument(skip(self), level = "trace")]
    pub fn read_discriminant(
        &self,
        op: &impl Readable<'tcx, M::Provenance>,
    ) -> InterpResult<'tcx, VariantIdx> {
        let ty = op.layout().ty;
        trace!("read_discriminant_value {:#?}", op.layout());
        // Get type and layout of the discriminant.
        let discr_layout = self.layout_of(ty.discriminant_ty(*self.tcx))?;
        trace!("discriminant type: {:?}", discr_layout.ty);

        // We use "discriminant" to refer to the value associated with a particular enum variant.
        // This is not to be confused with its "variant index", which is just determining its position in the
        // declared list of variants -- they can differ with explicitly assigned discriminants.
        // We use "tag" to refer to how the discriminant is encoded in memory, which can be either
        // straight-forward (`TagEncoding::Direct`) or with a niche (`TagEncoding::Niche`).
        let (tag_scalar_layout, tag_encoding, tag_field) = match op.layout().variants {
            Variants::Single { index } => {
                // Do some extra checks on enums.
                if ty.is_enum() {
                    // Hilariously, `Single` is used even for 0-variant enums.
                    // (See https://github.com/rust-lang/rust/issues/89765).
                    if matches!(ty.kind(), ty::Adt(def, ..) if def.variants().is_empty()) {
                        throw_ub!(UninhabitedEnumVariantRead(index))
                    }
                    // For consistency with `write_discriminant`, and to make sure that
                    // `project_downcast` cannot fail due to strange layouts, we declare immediate UB
                    // for uninhabited variants.
                    if op.layout().for_variant(self, index).abi.is_uninhabited() {
                        throw_ub!(UninhabitedEnumVariantRead(index))
                    }
                }
                return Ok(index);
            }
            Variants::Multiple { tag, ref tag_encoding, tag_field, .. } => {
                (tag, tag_encoding, tag_field)
            }
        };

        // There are *three* layouts that come into play here:
        // - The discriminant has a type for typechecking. This is `discr_layout`, and is used for
        //   the `Scalar` we return.
        // - The tag (encoded discriminant) has layout `tag_layout`. This is always an integer type,
        //   and used to interpret the value we read from the tag field.
        //   For the return value, a cast to `discr_layout` is performed.
        // - The field storing the tag has a layout, which is very similar to `tag_layout` but
        //   may be a pointer. This is `tag_val.layout`; we just use it for sanity checks.

        // Get layout for tag.
        let tag_layout = self.layout_of(tag_scalar_layout.primitive().to_int_ty(*self.tcx))?;

        // Read tag and sanity-check `tag_layout`.
        let tag_val = self.read_immediate(&self.project_field(op, tag_field)?)?;
        assert_eq!(tag_layout.size, tag_val.layout.size);
        assert_eq!(tag_layout.abi.is_signed(), tag_val.layout.abi.is_signed());
        trace!("tag value: {}", tag_val);

        // Figure out which discriminant and variant this corresponds to.
        let index = match *tag_encoding {
            TagEncoding::Direct => {
                // Generate a specific error if `tag_val` is not an integer.
                // (`tag_bits` itself is only used for error messages below.)
                let tag_bits = tag_val
                    .to_scalar()
                    .try_to_int()
                    .map_err(|dbg_val| err_ub!(InvalidTag(dbg_val)))?
                    .assert_bits(tag_layout.size);
                // Cast bits from tag layout to discriminant layout.
                // After the checks we did above, this cannot fail, as
                // discriminants are int-like.
                let discr_val = self.int_to_int_or_float(&tag_val, discr_layout).unwrap();
                let discr_bits = discr_val.to_scalar().assert_bits(discr_layout.size);
                // Convert discriminant to variant index, and catch invalid discriminants.
                let index = match *ty.kind() {
                    ty::Adt(adt, _) => {
                        adt.discriminants(*self.tcx).find(|(_, var)| var.val == discr_bits)
                    }
                    ty::Coroutine(def_id, args) => {
                        let args = args.as_coroutine();
                        args.discriminants(def_id, *self.tcx).find(|(_, var)| var.val == discr_bits)
                    }
                    _ => span_bug!(self.cur_span(), "tagged layout for non-adt non-coroutine"),
                }
                .ok_or_else(|| err_ub!(InvalidTag(Scalar::from_uint(tag_bits, tag_layout.size))))?;
                // Return the cast value, and the index.
                index.0
            }
            TagEncoding::Niche { untagged_variant, ref niche_variants, niche_start } => {
                let tag_val = tag_val.to_scalar();
                // Compute the variant this niche value/"tag" corresponds to. With niche layout,
                // discriminant (encoded in niche/tag) and variant index are the same.
                let variants_start = niche_variants.start().as_u32();
                let variants_end = niche_variants.end().as_u32();
                let variant = match tag_val.try_to_int() {
                    Err(dbg_val) => {
                        // So this is a pointer then, and casting to an int failed.
                        // Can only happen during CTFE.
                        // The niche must be just 0, and the ptr not null, then we know this is
                        // okay. Everything else, we conservatively reject.
                        let ptr_valid = niche_start == 0
                            && variants_start == variants_end
                            && !self.scalar_may_be_null(tag_val)?;
                        if !ptr_valid {
                            throw_ub!(InvalidTag(dbg_val))
                        }
                        untagged_variant
                    }
                    Ok(tag_bits) => {
                        let tag_bits = tag_bits.assert_bits(tag_layout.size);
                        // We need to use machine arithmetic to get the relative variant idx:
                        // variant_index_relative = tag_val - niche_start_val
                        let tag_val = ImmTy::from_uint(tag_bits, tag_layout);
                        let niche_start_val = ImmTy::from_uint(niche_start, tag_layout);
                        let variant_index_relative_val =
                            self.wrapping_binary_op(mir::BinOp::Sub, &tag_val, &niche_start_val)?;
                        let variant_index_relative =
                            variant_index_relative_val.to_scalar().assert_bits(tag_val.layout.size);
                        // Check if this is in the range that indicates an actual discriminant.
                        if variant_index_relative <= u128::from(variants_end - variants_start) {
                            let variant_index_relative = u32::try_from(variant_index_relative)
                                .expect("we checked that this fits into a u32");
                            // Then computing the absolute variant idx should not overflow any more.
                            let variant_index = VariantIdx::from_u32(
                                variants_start
                                    .checked_add(variant_index_relative)
                                    .expect("overflow computing absolute variant idx"),
                            );
                            let variants =
                                ty.ty_adt_def().expect("tagged layout for non adt").variants();
                            assert!(variant_index < variants.next_index());
                            variant_index
                        } else {
                            untagged_variant
                        }
                    }
                };
                // Compute the size of the scalar we need to return.
                // No need to cast, because the variant index directly serves as discriminant and is
                // encoded in the tag.
                variant
            }
        };
        // Reading the discriminant of an uninhabited variant is UB. This is the basis for the
        // `uninhabited_enum_branching` MIR pass. It also ensures consistency with
        // `write_discriminant`.
        if op.layout().for_variant(self, index).abi.is_uninhabited() {
            throw_ub!(UninhabitedEnumVariantRead(index))
        }
        Ok(index)
    }

    pub fn discriminant_for_variant(
        &self,
        ty: Ty<'tcx>,
        variant: VariantIdx,
    ) -> InterpResult<'tcx, ImmTy<'tcx, M::Provenance>> {
        let discr_layout = self.layout_of(ty.discriminant_ty(*self.tcx))?;
        let discr_value = match ty.discriminant_for_variant(*self.tcx, variant) {
            Some(discr) => {
                // This type actually has discriminants.
                assert_eq!(discr.ty, discr_layout.ty);
                Scalar::from_uint(discr.val, discr_layout.size)
            }
            None => {
                // On a type without actual discriminants, variant is 0.
                assert_eq!(variant.as_u32(), 0);
                Scalar::from_uint(variant.as_u32(), discr_layout.size)
            }
        };
        Ok(ImmTy::from_scalar(discr_value, discr_layout))
    }

    /// Computes how to write the tag of a given variant of enum `ty`:
    /// - `None` means that nothing needs to be done as the variant is encoded implicitly
    /// - `Some((val, field_idx))` means that the given integer value needs to be stored at the
    ///   given field index.
    pub(crate) fn tag_for_variant(
        &self,
        ty: Ty<'tcx>,
        variant_index: VariantIdx,
    ) -> InterpResult<'tcx, Option<(ScalarInt, usize)>> {
        match self.layout_of(ty)?.variants {
            abi::Variants::Single { index } => {
                assert_eq!(index, variant_index);
                Ok(None)
            }

            abi::Variants::Multiple {
                tag_encoding: TagEncoding::Direct,
                tag: tag_layout,
                tag_field,
                ..
            } => {
                // raw discriminants for enums are isize or bigger during
                // their computation, but the in-memory tag is the smallest possible
                // representation
                let discr = self.discriminant_for_variant(ty, variant_index)?;
                let discr_size = discr.layout.size;
                let discr_val = discr.to_scalar().to_bits(discr_size)?;
                let tag_size = tag_layout.size(self);
                let tag_val = tag_size.truncate(discr_val);
                let tag = ScalarInt::try_from_uint(tag_val, tag_size).unwrap();
                Ok(Some((tag, tag_field)))
            }

            abi::Variants::Multiple {
                tag_encoding: TagEncoding::Niche { untagged_variant, .. },
                ..
            } if untagged_variant == variant_index => {
                // The untagged variant is implicitly encoded simply by having a
                // value that is outside the niche variants.
                Ok(None)
            }

            abi::Variants::Multiple {
                tag_encoding:
                    TagEncoding::Niche { untagged_variant, ref niche_variants, niche_start },
                tag: tag_layout,
                tag_field,
                ..
            } => {
                assert!(variant_index != untagged_variant);
                let variants_start = niche_variants.start().as_u32();
                let variant_index_relative = variant_index
                    .as_u32()
                    .checked_sub(variants_start)
                    .expect("overflow computing relative variant idx");
                // We need to use machine arithmetic when taking into account `niche_start`:
                // tag_val = variant_index_relative + niche_start_val
                let tag_layout = self.layout_of(tag_layout.primitive().to_int_ty(*self.tcx))?;
                let niche_start_val = ImmTy::from_uint(niche_start, tag_layout);
                let variant_index_relative_val =
                    ImmTy::from_uint(variant_index_relative, tag_layout);
                let tag = self
                    .wrapping_binary_op(
                        mir::BinOp::Add,
                        &variant_index_relative_val,
                        &niche_start_val,
                    )?
                    .to_scalar()
                    .assert_int();
                Ok(Some((tag, tag_field)))
            }
        }
    }
}
