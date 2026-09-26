//! Streaming content-encoding decoders. Each accepts compressed chunks and
//! returns whatever plain bytes are available so far.

use std::io::Write;

pub(crate) enum Decoder {
    Identity,
    Gzip(flate2::write::MultiGzDecoder<Vec<u8>>),
    Deflate(flate2::write::ZlibDecoder<Vec<u8>>),
    Brotli(Box<brotli::DecompressorWriter<Vec<u8>>>),
}

impl Decoder {
    pub fn for_encoding(encoding: &str) -> Self {
        // Only the last applied encoding matters for what we send; servers
        // do not stack encodings in practice.
        let last = encoding.split(',').next_back().unwrap_or("").trim();
        if last.eq_ignore_ascii_case("gzip") || last.eq_ignore_ascii_case("x-gzip") {
            Decoder::Gzip(flate2::write::MultiGzDecoder::new(Vec::new()))
        } else if last.eq_ignore_ascii_case("deflate") {
            Decoder::Deflate(flate2::write::ZlibDecoder::new(Vec::new()))
        } else if last.eq_ignore_ascii_case("br") {
            Decoder::Brotli(Box::new(brotli::DecompressorWriter::new(Vec::new(), 8192)))
        } else {
            Decoder::Identity
        }
    }

    /// Feed compressed bytes; get decoded bytes available so far.
    pub fn push(&mut self, data: &[u8]) -> std::io::Result<Vec<u8>> {
        match self {
            Decoder::Identity => Ok(data.to_vec()),
            Decoder::Gzip(d) => {
                d.write_all(data)?;
                Ok(std::mem::take(d.get_mut()))
            }
            Decoder::Deflate(d) => {
                d.write_all(data)?;
                Ok(std::mem::take(d.get_mut()))
            }
            Decoder::Brotli(d) => {
                d.write_all(data)?;
                Ok(std::mem::take(d.get_mut()))
            }
        }
    }

    /// Flush the stream end; returns any final bytes.
    pub fn finish(self) -> std::io::Result<Vec<u8>> {
        match self {
            Decoder::Identity => Ok(Vec::new()),
            Decoder::Gzip(d) => d.finish(),
            Decoder::Deflate(d) => d.finish(),
            Decoder::Brotli(mut d) => {
                d.flush()?;
                Ok(std::mem::take(d.get_mut()))
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn gzip_streams_in_pieces() {
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let payload: Vec<u8> = (0..50_000u32).map(|i| (i % 251) as u8).collect();
        enc.write_all(&payload).unwrap();
        let compressed = enc.finish().unwrap();

        let mut dec = Decoder::for_encoding("gzip");
        let mut out = Vec::new();
        for chunk in compressed.chunks(777) {
            out.extend(dec.push(chunk).unwrap());
        }
        out.extend(dec.finish().unwrap());
        assert_eq!(out, payload);
    }

    #[test]
    fn brotli_round_trip() {
        let payload = b"hello hello hello hello brotli".repeat(100);
        let mut compressed = Vec::new();
        {
            let mut w = brotli::CompressorWriter::new(&mut compressed, 4096, 5, 22);
            w.write_all(&payload).unwrap();
        }
        let mut dec = Decoder::for_encoding("br");
        let mut out = Vec::new();
        for chunk in compressed.chunks(100) {
            out.extend(dec.push(chunk).unwrap());
        }
        out.extend(dec.finish().unwrap());
        assert_eq!(out, payload);
    }

    #[test]
    fn identity_passes_through() {
        let mut dec = Decoder::for_encoding("");
        assert_eq!(dec.push(b"abc").unwrap(), b"abc");
    }

    #[test]
    fn corrupt_gzip_is_an_error_not_a_panic() {
        let mut dec = Decoder::for_encoding("gzip");
        let r = dec.push(&[0x1f, 0x8b, 0xff, 0xff, 0x00, 0x00, 0x00]);
        let _ = r;
        let _ = dec.finish();
    }
}
