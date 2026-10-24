"""SentencePiece Unigram decoding using the pinned tokenizer's normalization map."""
import base64
import json
import re
import struct
from pathlib import Path


class Tokenizer:
    """Implements the pinned SigLIP tokenizer, including its Darts normalization trie."""

    def __init__(self, path: Path) -> None:
        data = json.loads(path.read_text())
        normalizer = data["normalizer"]["normalizers"][0]
        compiled = base64.b64decode(normalizer["precompiled_charsmap"])
        size = struct.unpack_from("<I", compiled)[0]
        self.units = struct.unpack_from(f"<{size // 4}I", compiled, 4)
        self.strings = compiled[4 + size:]
        self.vocab: dict[str, tuple[int, float]] = {
            piece: (i, float(score)) for i, (piece, score) in enumerate(data["model"]["vocab"])
            if i >= 259
        }
        self.max_piece = max(map(len, self.vocab))
        self.unknown_score = min(score for _, score in self.vocab.values()) - 10

    @staticmethod
    def offset(unit: int) -> int:
        return (unit >> 10) << ((unit & (1 << 9)) >> 6)

    def normalize(self, value: str) -> str:
        source = value.encode("utf-8")
        output = bytearray()
        index = 0
        while index < len(source):
            node = self.offset(self.units[0])
            match: tuple[int, int] | None = None
            for end in range(index, len(source)):
                node ^= source[end]
                if node >= len(self.units):
                    break
                unit = self.units[node]
                if unit & ((1 << 31) | 255) != source[end]:
                    break
                node ^= self.offset(unit)
                if unit & (1 << 8):
                    match = (end + 1, self.units[node] & 0x7fffffff)
            if match:
                end, replacement = match
                stop = self.strings.index(b"\0", replacement)
                output.extend(self.strings[replacement:stop])
                index = end
            else:
                output.append(source[index])
                index += 1
        return re.sub(" {2,}", " ", output.decode("utf-8"))

    def encode(self, value: str) -> list[int]:
        """Produce exactly 64 input ids with EOS and padding id 1, as SigLIP expects."""
        value = "▁" + self.normalize(value).replace(" ", "▁")
        scores = [float("-inf")] * (len(value) + 1)
        previous: list[tuple[int, int]] = [(0, 2)] * (len(value) + 1)
        scores[0] = 0
        for start in range(len(value)):
            matched_single = False
            for end in range(start + 1, min(len(value), start + self.max_piece) + 1):
                found = self.vocab.get(value[start:end])
                if found is None:
                    continue
                identifier, score = found
                matched_single |= end == start + 1
                if scores[start] + score > scores[end]:
                    scores[end] = scores[start] + score
                    previous[end] = (start, identifier)
            if not matched_single and scores[start] + self.unknown_score > scores[start + 1]:
                scores[start + 1] = scores[start] + self.unknown_score
                previous[start + 1] = (start, 2)
        ids = []
        end = len(value)
        while end:
            end, identifier = previous[end]
            ids.append(identifier)
        ids.reverse()
        ids = ids[:63] + [1]
        return ids + [1] * (64 - len(ids))
