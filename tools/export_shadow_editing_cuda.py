#!/usr/bin/env python3
"""Derive fixed-shape CUDA graphs from pinned, unmodified editing weights.

This CPU-only export folds shape calculations, never runs image inference,
and never activates a deployment. Use requirements-editing-export.txt in a
separate environment; the installed CUDA Runtime library remains independent.
"""
from __future__ import annotations

import argparse
import json
from pathlib import Path
import tempfile

from prepare_shadow_vision import REPOSITORY, sha256

EXPORT_IDENTITY = "infer-editing-static-cuda-v1-onnx1.19.0-ort1.27.0-basic"
CUDA_DINO_SOURCE = {
    "sha256": "267c8b00d269dc2dda0d8592812d3027abd5fb59f1cabf8e3d56f8fdf07f68ec",
    "size_bytes": 718761381,
    "source_url": "https://huggingface.co/onnx-community/grounding-dino-tiny-ONNX/resolve/ff690b0a8050566c290287545bd059350f3e9096/onnx/model.onnx",
    "source_revision": "onnx-community/grounding-dino-tiny-ONNX@ff690b0a8050566c290287545bd059350f3e9096",
    "license_spdx": "Apache-2.0",
}
CUDA_BUILDS = {
    "bisenet_resnet18_face_parsing_onnx_cuda_static_v1": {
        "reference": "bisenet_resnet18_face_parsing_onnx_cpu_v1",
        "source": "bisenet_resnet18_face_parsing_onnx_cpu_v1/model.onnx",
        "source_sha256": "0d9bd318e46987c3bdbfacae9e2c0f461cae1c6ac6ea6d43bbe541a91727e33f",
        "shapes": {"input": [1, 3, 512, 512]},
        "size_bytes": 53198514,
        "sha256": "c3ceb7e003ed43ae920406641e452df62d68074e8dea660aa911a26297d32cf2",
    },
    "lama_inpainting_onnx_cuda_static_fp32_v1": {
        "reference": "lama_inpainting_onnx_v1",
        "source": "lama_inpainting_onnx_v1/model.onnx",
        "source_sha256": "7df918ac3921d3daf0aae1d219776cf0dc4e4935f035af81841b40adcf74fdf2",
        "shapes": {"image": [1, 3, 512, 512], "mask": [1, 1, 512, 512]},
        "size_bytes": 210531468,
        "sha256": "61443755b58a753735dd26ef3292fc9a84ecb7124d792402529753124bb56a5f",
    },
    "grounding_dino_tiny_onnx_cuda_static_fp32_v1": {
        "reference": "grounding_dino_tiny_onnx_int8_v1",
        "source": "grounding_dino_tiny_onnx_cuda_fp32_v1/model.onnx",
        "source_sha256": CUDA_DINO_SOURCE["sha256"],
        "shapes": {"pixel_values": [1, 3, 800, 800], "input_ids": [1, 256],
                   "token_type_ids": [1, 256], "attention_mask": [1, 256], "pixel_mask": [1, 800, 800]},
        "size_bytes": 833500506,
        "sha256": "5729a3f11d0b126f53a342f7852b3e370a18fbf09afe42980330b83741f5bd22",
    },
}


def _expand_lama_weights(model, ort):
    """Evaluate only constant blocked INT8 dequantization during export."""
    from onnx import TensorProto, helper, numpy_helper
    initial = {value.name: value for value in model.graph.initializer}
    nodes = [node for node in model.graph.node if node.op_type == "DequantizeLinear"]
    if len(nodes) != 299 or any(any(value not in initial for value in node.input) for node in nodes):
        raise ValueError("LaMa constant dequantization contract changed")
    used = {value for node in nodes for value in node.input}
    outputs = [helper.make_tensor_value_info(node.output[0], TensorProto.FLOAT,
                                             list(initial[node.input[0]].dims)) for node in nodes]
    graph = helper.make_graph(nodes, "constant-weight-dequantization", [], outputs,
                              [value for value in model.graph.initializer if value.name in used])
    constant_model = helper.make_model(graph, opset_imports=list(model.opset_import))
    constant_model.ir_version = model.ir_version
    options = ort.SessionOptions()
    options.log_severity_level = 3
    session = ort.InferenceSession(constant_model.SerializeToString(), sess_options=options,
                                  providers=["CPUExecutionProvider"])
    values = session.run(None, {})
    del session, constant_model
    names = {node.name for node in nodes}
    remaining = [node for node in model.graph.node if node.name not in names]
    del model.graph.node[:]
    model.graph.node.extend(remaining)
    model.graph.initializer.extend(numpy_helper.from_array(value, output.name)
                                   for value, output in zip(values, outputs, strict=True))
    # Old quantized constants have no consumers after their exact values become
    # FP32 initializers. The optimizer will remove any other unused constants.
    referenced = {value for node in remaining for value in node.input}
    kept = [value for value in model.graph.initializer if value.name in referenced]
    del model.graph.initializer[:]
    model.graph.initializer.extend(kept)


def _grounding_cuda_control(model):
    """Preserve exact shape/boolean results using CUDA-supported kernels."""
    import onnx
    from onnx import TensorProto, helper, numpy_helper
    by_output = {value: node for node in model.graph.node for value in node.output}
    initial = {value.name: numpy_helper.to_array(value) for value in model.graph.initializer}
    removed, replacements = set(), {}
    # Exported expand(-1) guards compare a runtime Shape with -1. Runtime
    # dimensions are always nonnegative, so these two Where results equal Shape.
    for name in ("/model/Where_7", "/model/Where_10"):
        node = next(node for node in model.graph.node if node.name == name)
        equal = by_output[node.input[0]]
        if (equal.op_type != "Equal" or equal.input[0] != node.input[2]
                or by_output[equal.input[0]].op_type != "Shape"
                or not (initial[equal.input[1]] == -1).all()
                or sum(equal.output[0] in other.input for other in model.graph.node) != 1):
            raise ValueError("Grounding DINO shape guard differs from the fixed export")
        removed.update((node.name, equal.name))
        replacements[node.output[0]] = node.input[2]
    tile_names = {
        "/model/encoder/layers.0/fusion_layer/attn/Tile_1",
        "/model/encoder/layers.0/fusion_layer/attn/Tile",
        "/model/encoder/layers.0/text_enhancer_layer/Tile",
        "/model/decoder/Tile",
    }
    nodes, changed = [], set()
    for node in model.graph.node:
        if node.name in removed:
            continue
        for index, value in enumerate(node.input):
            node.input[index] = replacements.get(value, value)
        if node.name in tile_names:
            # bool -> float32 -> Tile -> bool is exact for both 0 and 1.
            # No learned value or attention decision is approximated.
            before, after = node.input[0], node.output[0]
            node.input[0] = node.name + "__float_input"
            node.output[0] = node.name + "__float_output"
            nodes.extend((helper.make_node("Cast", [before], [node.input[0]], to=TensorProto.FLOAT,
                                           name=node.name + "__input_float"), node,
                          helper.make_node("Cast", [node.output[0]], [after], to=TensorProto.BOOL,
                                           name=node.name + "__output_bool")))
            changed.add(node.name)
        else:
            nodes.append(node)
    if changed != tile_names:
        raise ValueError("Grounding DINO boolean tiles differ from the fixed export")
    del model.graph.node[:]
    model.graph.node.extend(nodes)
    onnx.checker.check_model(model)


def export(source_root: Path, output_root: Path, builds):
    import onnx
    import onnxruntime as ort
    if onnx.__version__ != "1.19.0" or ort.__version__ != "1.27.0":
        raise ValueError("export requires onnx 1.19.0 and CPU onnxruntime 1.27.0")
    if ort.get_available_providers() != ["AzureExecutionProvider", "CPUExecutionProvider"]:
        raise ValueError("use the separate CPU export environment")
    receipts = []
    for name in builds:
        contract = CUDA_BUILDS[name]
        source = source_root / contract["source"]
        if sha256(source) != contract["source_sha256"]:
            raise ValueError("source graph differs from the pinned identity: " + name)
        destination = output_root / name
        destination.mkdir(parents=True, exist_ok=True)
        target = destination / "model.onnx"
        if target.exists():
            if not contract["sha256"] or sha256(target) != contract["sha256"]:
                raise ValueError("existing derived graph differs from the fixed identity: " + name)
        else:
            with tempfile.TemporaryDirectory(prefix=".editing-export-", dir=output_root) as scratch:
                scratch = Path(scratch)
                model = onnx.load_model(source)
                if {value.name for value in model.graph.input} != set(contract["shapes"]):
                    raise ValueError("source graph input names changed")
                for value in model.graph.input:
                    shape = contract["shapes"][value.name]
                    if len(value.type.tensor_type.shape.dim) != len(shape):
                        raise ValueError("source graph input rank changed")
                    for dim, size in zip(value.type.tensor_type.shape.dim, shape):
                        dim.ClearField("dim_param")
                        dim.dim_value = size
                if name.startswith("lama_"):
                    _expand_lama_weights(model, ort)
                frozen, optimized = scratch / "input-static.onnx", scratch / "basic.onnx"
                onnx.save_model(model, frozen)
                del model
                options = ort.SessionOptions()
                options.log_severity_level = 3
                # Basic optimization avoids CPU-specific layout/fused operators.
                options.graph_optimization_level = ort.GraphOptimizationLevel.ORT_ENABLE_BASIC
                options.optimized_model_filepath = str(optimized)
                session = ort.InferenceSession(str(frozen), sess_options=options, providers=["CPUExecutionProvider"])
                del session
                if name.startswith("grounding_dino"):
                    model = onnx.load_model(optimized)
                    _grounding_cuda_control(model)
                    onnx.save_model(model, optimized)
                    del model
                else:
                    onnx.checker.check_model(str(optimized))
                actual, size = sha256(optimized), optimized.stat().st_size
                if contract["sha256"] and (actual != contract["sha256"] or size != contract["size_bytes"]):
                    raise ValueError("derived graph differs from the fixed identity: " + name)
                optimized.replace(target)
        receipts.append({"build": name, "source_sha256": contract["source_sha256"],
                         "sha256": sha256(target), "size_bytes": target.stat().st_size})
        print(json.dumps(receipts[-1]), flush=True)
    (output_root / "export-manifest.json").write_text(json.dumps({
        "export_identity": EXPORT_IDENTITY, "builds": receipts}, indent=2) + "\n", encoding="utf-8")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source-root", required=True, type=Path)
    parser.add_argument("--output-root", required=True, type=Path)
    parser.add_argument("--build", action="append", choices=CUDA_BUILDS)
    args = parser.parse_args()
    for root in (args.source_root, args.output_root):
        root = root.resolve()
        if root == REPOSITORY or REPOSITORY in root.parents:
            parser.error("model sources and exports must stay outside the source checkout")
    args.output_root.mkdir(parents=True, exist_ok=True)
    export(args.source_root, args.output_root, args.build or list(CUDA_BUILDS))


if __name__ == "__main__":
    main()
