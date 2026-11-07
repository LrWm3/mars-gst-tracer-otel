//! Conservative display aliases for observed NVIDIA-generated names. This is
//! separate from C++ demangling: unknown families/fields keep the raw spelling.
//! The caller appends a stable identifier and retains the complete system_name.

pub(super) fn format(raw: &str) -> Option<String> {
    sm80(raw).or_else(|| trt(raw))
}

fn number(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 6
        && value.bytes().all(|b| b.is_ascii_digit())
        && value.parse::<u32>().is_ok_and(|n| n > 0)
}

fn dimensions(value: &str, count: usize) -> bool {
    value.split('x').count() == count && value.split('x').all(number)
}

fn sm80(raw: &str) -> Option<String> {
    let rest = raw.strip_prefix("sm80_xmma_fprop_implicit_gemm_")?;
    let (rest, indexed) = match rest.strip_prefix("indexed_wo_smem_") {
        Some(rest) => (rest, ", indexed_wo_smem"),
        None => (rest, ""),
    };
    // Only summarize the observed all-half precision variants; do not infer
    // operand/accumulator types for unfamiliar mixed-precision spellings.
    let rest = rest.strip_prefix("f16f16_f16f16_")?;
    let rest = rest.strip_prefix("f16_").unwrap_or(rest);
    let rest = rest.strip_prefix("nhwckrsc_nhwc_tilesize")?;
    let rest = rest.strip_suffix("_execute_kernel_trt")?;
    let mut fields = rest.split('_');
    let tile = fields.next()?;
    let stage = fields.next()?.strip_prefix("stage")?;
    let warp = fields.next()?.strip_prefix("warpsize")?;
    let group = fields.next()?.strip_prefix('g')?;
    let tensor = fields.next()?.strip_prefix("tensor")?;
    if !dimensions(tile, 3)
        || !number(stage)
        || !dimensions(warp, 3)
        || !number(group)
        || !dimensions(tensor, 3)
        || !fields.all(|field| {
            matches!(field, "simple" | "t1r1s1" | "linkable")
                || field.strip_prefix("aligna").is_some_and(number)
                || field.strip_prefix("alignc").is_some_and(number)
        })
    {
        return None;
    }
    Some(format!(
        "SM80 XMMA implicit GEMM fprop (FP16, NHWC, tile {tile}{indexed})"
    ))
}

fn trt(raw: &str) -> Option<String> {
    let tile = raw
        .strip_prefix("trt_ampere_h1688cudnn_")?
        .strip_suffix("_ldg8_relu_exp_interior_nhwc_linkable_tn_v1")?;
    if !dimensions(tile, 2) {
        return None;
    }
    // h1688 is retained as an implementation tag. In particular, do not
    // interpret "exp" as an exponential activation or claim a model layer.
    Some(format!(
        "TensorRT cuDNN h1688 (Ampere, ReLU, NHWC, tile {tile})"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SM80: &str = "sm80_xmma_fprop_implicit_gemm_f16f16_f16f16_f16_nhwckrsc_nhwc_tilesize128x32x32_stage4_warpsize4x1x1_g1_tensor16x8x16_simple_t1r1s1_linkable_execute_kernel_trt";
    const TRT: &str = "trt_ampere_h1688cudnn_256x64_ldg8_relu_exp_interior_nhwc_linkable_tn_v1";

    #[test]
    fn observed_families_expose_useful_fields_without_guessing_private_tokens() {
        assert_eq!(
            format(SM80).unwrap(),
            "SM80 XMMA implicit GEMM fprop (FP16, NHWC, tile 128x32x32)"
        );
        let indexed = SM80
            .replace("gemm_", "gemm_indexed_wo_smem_")
            .replace("_f16_nhwc", "_nhwc");
        assert!(format(&indexed).unwrap().ends_with(", indexed_wo_smem)"));
        assert_eq!(
            format(TRT).unwrap(),
            "TensorRT cuDNN h1688 (Ampere, ReLU, NHWC, tile 256x64)"
        );
        assert!(format(&TRT.replace("256x64", "128x128"))
            .unwrap()
            .contains("tile 128x128"));
    }

    #[test]
    fn unknown_or_malformed_families_are_not_mislabelled() {
        for name in [
            "generatedNativePointwise".to_owned(),
            "_Z6kernelPi".to_owned(),
            SM80.replace("sm80", "sm90"),
            SM80.replace("f16f16", "f16f32"),
            SM80.replace("128x32x32", "128x0x32"),
            SM80.replace("128x32x32", "128x32"),
            SM80.replace("stage4", "stageUnknown"),
            SM80.replace("simple", "unknown"),
            SM80.replace("_execute_kernel_trt", ""),
            TRT.replace("relu", "gelu"),
            TRT.replace("h1688", "unknown"),
            TRT.replace("256x64", "256x64x32"),
            TRT.replace("tn_v1", "tn_v2"),
        ] {
            assert!(format(&name).is_none(), "{name}");
        }
    }
}
