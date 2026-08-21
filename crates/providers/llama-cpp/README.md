# qmt-llama-cpp

QueryMT provider backed by `llama.cpp`.

## Experimental DFlash2 support

This branch pins `querymt/llama-cpp-rs` to a revision containing llama.cpp PR
`ggml-org/llama.cpp#27342`. Configure DFlash/DFlash2 through the unified
`speculative` object:

```yaml
provider: llama_cpp
model: ggml-org/Qwen3.8-27B-GGUF:Q4_K_M
speculative:
  type: draft-dflash
  model: incoai/Qwen3.8-27B-DFlash2-GGUF:Q4_K_M
  n_max: 7
  n_min: 0
  p_min: 0.0
```

The fields map to llama-server as follows:

- `model` corresponds to `-hf`.
- `speculative.model` corresponds to `-hfd` / `--model-draft`.
- `type: draft-dflash` corresponds to `--spec-type draft-dflash`.
- `n_max` corresponds to `--spec-draft-n-max`.

DFlash2 is detected from the draft GGUF metadata; it does not use a separate
configuration type. The shared default for `n_max` is 3, while the Qwen3.8
example explicitly uses 7. Benchmark different values for each model/backend.

Speculative decoding supports regular, streaming, structured-output, and tool
sampling. Requests containing media fall back to ordinary multimodal decoding.
The provider logs drafted and accepted token counts for performance checks.

MTP uses the same API:

```yaml
speculative:
  type: mtp
  n_max: 3
```

Set `speculative.model` when MTP tensors are supplied in a separate sidecar;
omit it when they are bundled in the target GGUF.
