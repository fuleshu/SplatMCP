// Artifact identity for captured frames.
//
// Every artifact the capture contract reports carries a checksum, so a caller can tell two
// images apart without comparing bytes. The value is FNV-1a 64 over the encoded bytes - the same
// function `splatmcp-core` records for an export - so it is measured the same way on both sides
// of the bridge.
//
// Two details matter to a JavaScript producer:
//
// - a u64 does not survive a `Number`, so the value travels as its exact decimal digits in a
//   string. The core accepts a number or digits, and a checksum that silently lost its low bits
//   would identify nothing.
// - a frame is megabytes, so the digest stays a plain loop over bytes. Nothing here allocates
//   per byte, and no `BigInt` is created inside the loop.

/** Algorithm every artifact checksum in this project is computed with. */
export const CHECKSUM_ALGORITHM = "fnv1a64";

const FNV_OFFSET_BASIS = 0xcbf29ce484222325n;
const FNV_PRIME = 0x100000001b3n;
const MASK_64 = 0xffffffffffffffffn;

/** FNV-1a 64 of `bytes`, as a `BigInt` so no digit is lost on the way to the wire. */
export function fnv1a64(bytes) {
  let hash = FNV_OFFSET_BASIS;
  for (let index = 0; index < bytes.length; index += 1) {
    hash = ((hash ^ BigInt(bytes[index])) * FNV_PRIME) & MASK_64;
  }
  return hash;
}

/** The contract's checksum: the algorithm, the exact digits, and the byte count. */
export function checksumSummary(bytes) {
  return {
    algorithm: CHECKSUM_ALGORITHM,
    value: fnv1a64(bytes).toString(10),
    bytes: bytes.length,
  };
}

/** Checksum of an artifact the bridge carries as base64. */
export function checksumOfBase64(base64) {
  return checksumSummary(base64ToBytes(base64));
}

/** Decodes base64, in a browser or in node. */
export function base64ToBytes(base64) {
  const text = String(base64 ?? "");
  if (typeof atob === "function") {
    const binary = atob(text);
    const bytes = new Uint8Array(binary.length);
    for (let index = 0; index < binary.length; index += 1) {
      bytes[index] = binary.charCodeAt(index);
    }
    return bytes;
  }
  if (typeof Buffer !== "undefined") {
    return new Uint8Array(Buffer.from(text, "base64"));
  }
  throw new Error("this environment has no base64 decoder");
}

/** Encodes bytes as base64, in a browser or in node. */
export function bytesToBase64(bytes) {
  if (typeof btoa === "function") {
    let binary = "";
    // One call per chunk: `String.fromCharCode(...bytes)` on a megabyte frame overflows the
    // argument list, which is why the chunk size is fixed instead of the whole array.
    const step = 0x8000;
    for (let index = 0; index < bytes.length; index += step) {
      binary += String.fromCharCode.apply(null, bytes.subarray(index, index + step));
    }
    return btoa(binary);
  }
  if (typeof Buffer !== "undefined") {
    return Buffer.from(bytes).toString("base64");
  }
  throw new Error("this environment has no base64 encoder");
}

/** Splits a `data:` URL into its media type, its base64 payload and the bytes it carries. */
export function decodeDataUrl(dataUrl) {
  const text = String(dataUrl ?? "");
  const comma = text.indexOf(",");
  if (comma < 0) {
    throw new Error("the canvas returned an unusable image");
  }
  const header = text.slice(0, comma);
  const separator = header.indexOf(";");
  const mimeType = header.slice(
    header.indexOf(":") + 1,
    separator < 0 ? header.length : separator,
  );
  const base64 = text.slice(comma + 1);
  return { mime_type: mimeType, base64, bytes: base64ToBytes(base64) };
}

/** The `data:` URL a browser needs to draw or download an encoded artifact. */
export function dataUrlFor(mimeType, base64) {
  return `data:${mimeType};base64,${base64}`;
}
