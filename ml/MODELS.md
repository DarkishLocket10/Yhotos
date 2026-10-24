# Locked model assets

All artifacts are downloaded and SHA-256 verified at build time, then verified
again at startup. Runtime has no download path. `models.lock` pins revisions and
full digests; the container copies the face licence notices in `licenses/`.

| Assets | Model | Licence / upstream |
| --- | --- | --- |
| siglip-vision.onnx, siglip-text.onnx, tokenizer.json | SigLIP ViT-B/16, 768 dimensions | Apache-2.0, [Google SigLIP](https://huggingface.co/google/siglip-base-patch16-224), [ONNX export](https://huggingface.co/Xenova/siglip-base-patch16-224/tree/4649052661e53c7000355844105f8a1792088239) |
| face_detection_yunet_2023mar.onnx | YuNet detector, five landmarks | MIT, [OpenCV Zoo notice](https://github.com/opencv/opencv_zoo/blob/f12e12798e8314f7c074a6656816c048dcc95b7a/models/face_detection_yunet/LICENSE) |
| face_recognition_sface_2021dec.onnx | SFace, 128 dimensions | Apache-2.0, [OpenCV Zoo notice](https://github.com/opencv/opencv_zoo/blob/f12e12798e8314f7c074a6656816c048dcc95b7a/models/face_recognition_sface/LICENSE) |
| ocr-det.onnx, ocr-rec.onnx | PP-OCRv4 from RapidOCR 1.4.4 | Apache-2.0, [PaddleOCR](https://github.com/PaddlePaddle/PaddleOCR/blob/main/LICENSE) |

Face graphs run directly in ONNX Runtime. YuNet uses BGR 0..255 input at its
fixed 640x640 shape, with stride 8/16/32 decoding and NMS. Five-point similarity
alignment uses NumPy and Pillow; SFace takes RGB 0..255 at 112x112 and performs
normalization in its graph. No OpenCV API is used for faces. The pinned headless
OpenCV package is required by RapidOCR's text preprocessing and geometry; the
desktop opencv-python dependency is deliberately replaced in both closures.

Clustering uses unit vectors and cosine distance, min_samples=2, eps=0.637.
This starts at OpenCV's [published SFace LFW same-person cosine similarity
threshold 0.363](https://docs.opencv.org/4.x/d0/dd4/tutorial_dnn_face.html),
converted as distance = 1 - similarity. This is a published baseline, not a
claim of gallery-specific calibration. Hidden identities stay excluded, and
two named identities never merge automatically. Replays of completed input/model
pairs leave face ids and assignments intact, including zero-face completions.
The dimension migration retains old face ids/boxes/person decisions while
invalidating incompatible legacy vectors; first SFace processing reconciles
boxes at IoU >=0.8 without discarding unmatched assigned faces.

Docker offline full-pipeline validation and combined four-job GPU timing on
tower remain task 023 staging checks, per the maintainer decision. CPU checks
establish correctness only, not the 400 ms GPU acceptance budget.
