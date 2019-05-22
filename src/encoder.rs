extern crate crc32fast;
extern crate deflate;

use std::borrow::Cow;
use std::error;
use std::fmt;
use std::io::{self, Read, Write};
use std::mem;
use std::result;

use crc32fast::Hasher as Crc32;

use crate::chunk;
use crate::common::{Info, ColorType, BitDepth, Compression};
use crate::filter::{FilterType, filter};
use crate::traits::{WriteBytesExt, HasParameters, Parameter};

pub type Result<T> = result::Result<T, EncodingError>;

#[derive(Debug)]
pub enum EncodingError {
    IoError(io::Error),
    Format(Cow<'static, str>),
}

impl error::Error for EncodingError {
    fn description(&self) -> &str {
        use self::EncodingError::*;
        match *self {
            IoError(ref err) => err.description(),
            Format(ref desc) => &desc,
        }
    }
}

impl fmt::Display for EncodingError {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> result::Result<(), fmt::Error> {
        write!(fmt, "{}", (self as &error::Error).description())
    }
}

impl From<io::Error> for EncodingError {
    fn from(err: io::Error) -> EncodingError {
        EncodingError::IoError(err)
    }
}
impl From<EncodingError> for io::Error {
    fn from(err: EncodingError) -> io::Error {
        io::Error::new(io::ErrorKind::Other, (&err as &error::Error).description())
    }
}

/// PNG Encoder
pub struct Encoder<W: Write> {
    w: W,
    info: Info,
}

impl<W: Write> Encoder<W> {
    pub fn new(w: W, width: u32, height: u32) -> Encoder<W> {
        let mut info = Info::default();
        info.width = width;
        info.height = height;
        Encoder { w: w, info: info }
    }

    pub fn set_palette(&mut self, palette: Vec<u8>) {
        self.info.palette = Some(palette);
    }

    pub fn write_header(self) -> Result<Writer<W>> {
        Writer::new(self.w, self.info).init()
    }
}

impl<W: Write> HasParameters for Encoder<W> {}


impl<W: Write> Parameter<Encoder<W>> for ColorType {
    fn set_param(self, this: &mut Encoder<W>) {
        this.info.color_type = self
    }
}

impl<W: Write> Parameter<Encoder<W>> for BitDepth {
    fn set_param(self, this: &mut Encoder<W>) {
        this.info.bit_depth = self
    }
}

/// Set compression param for a ```Compression``` or any type that can transform 
/// into a ```Compression```, notably ```deflate::Compression``` and 
/// ```deflate::CompressionOptions``` which "just work".
impl<W: Write, C: Into<Compression>> Parameter<Encoder<W>> for C {
    fn set_param(self, this: &mut Encoder<W>) {
        this.info.compression = self.into()
    }
}

impl <W: Write> Parameter<Encoder<W>> for FilterType {
    fn set_param(self, this: &mut Encoder<W>) {
        this.info.filter = self
    }
}

/// PNG writer
pub struct Writer<W: Write> {
    w: W,
    info: Info,
}

fn write_chunk<W: Write>(w: &mut W, name: [u8; 4], data: &[u8]) -> Result<()> {
    w.write_be(data.len() as u32)?;
    w.write_all(&name)?;
    w.write_all(data)?;
    let mut crc = Crc32::new();
    crc.update(&name);
    crc.update(data);
    w.write_be(crc.finalize())?;
    Ok(())
}

impl<W: Write> Writer<W> {
    fn new(w: W, info: Info) -> Writer<W> {
        let w = Writer { w: w, info: info };
        w
    }

    fn init(mut self) -> Result<Self> {
        self.w.write_all(&[137, 80, 78, 71, 13, 10, 26, 10])?;
        let mut data = [0; 13];
        (&mut data[..]).write_be(self.info.width)?;
        (&mut data[4..]).write_be(self.info.height)?;
        data[8] = self.info.bit_depth as u8;
        data[9] = self.info.color_type as u8;
        data[12] = if self.info.interlaced { 1 } else { 0 };
        self.write_chunk(chunk::IHDR, &data)?;

        if let Some(p) = &self.info.palette {
            write_chunk(&mut self.w, chunk::PLTE, p)?;
        };

        Ok(self)
    }

    pub fn write_chunk(&mut self, name: [u8; 4], data: &[u8]) -> Result<()> {
        write_chunk(&mut self.w, name, data)
    }

    /// Writes the image data.
    pub fn write_image_data(&mut self, data: &[u8]) -> Result<()> {
        if self.info.color_type == ColorType::Indexed && self.info.palette.is_none() {
            return Err(EncodingError::Format(
                "can't write indexed image without palette".into())
            );
        }
        let bpp = self.info.bytes_per_pixel();
        let in_len = self.info.raw_row_length() - 1;
        let mut prev = vec![0; in_len];
        let mut current = vec![0; in_len];
        let data_size = in_len * self.info.height as usize;
        if data_size != data.len() {
            let message = format!("wrong data size, expected {} got {}", data_size, data.len());
            return Err(EncodingError::Format(message.into()));
        }
        let mut zlib = deflate::write::ZlibEncoder::new(Vec::new(), self.info.compression.clone());
        let filter_method = self.info.filter;
        for line in data.chunks(in_len) {
            current.copy_from_slice(&line);
            zlib.write_all(&[filter_method as u8])?;
            filter(filter_method, bpp, &prev, &mut current);
            zlib.write_all(&current)?;
            mem::swap(&mut prev, &mut current);
        }
        self.write_chunk(chunk::IDAT, &zlib.finish()?)
    }

    /// Create an stream writer.
    ///
    /// This allows you create images that do not fit
    /// in memory. The default chunk size is 4K, use
    /// `stream_writer_with_size` to set another chuck
    /// size.
    pub fn stream_writer(&mut self) -> StreamWriter<W> {
        self.stream_writer_with_size(4 * 1024)
    }

    /// Create a stream writer with custom buffer size.
    ///
    /// See `stream_writer`
    pub fn stream_writer_with_size(&mut self, size: usize) -> StreamWriter<W> {
        StreamWriter::new(self, size)
    }
}

impl<W: Write> Drop for Writer<W> {
    fn drop(&mut self) {
        let _ = self.write_chunk(chunk::IEND, &[]);
    }
}

struct ChunkWriter<'a, W: Write> {
    writer: &'a mut Writer<W>,
    buffer: Vec<u8>,
    index: usize,
}

impl<'a, W: Write> ChunkWriter<'a, W> {
    fn new(writer: &'a mut Writer<W>, buf_len: usize) -> ChunkWriter<'a, W> {
        ChunkWriter {
            writer,
            buffer: vec![0; buf_len],
            index: 0,
        }
    }
}

impl<'a, W: Write> Write for ChunkWriter<'a, W> {
    fn write(&mut self, mut buf: &[u8]) -> io::Result<usize> {
        let written = buf.read(&mut self.buffer[self.index..])?;
        self.index += written;
        
        if self.index + 1 >= self.buffer.len() {
            self.writer.write_chunk(chunk::IDAT, &self.buffer)?;
            self.index = 0;
        }

        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.index > 0 {
            self.writer.write_chunk(chunk::IDAT, &self.buffer[..self.index+1])?;
        }
        self.index = 0;
        Ok(())
    }
}

impl<'a, W: Write> Drop for ChunkWriter<'a, W> {
    fn drop(&mut self) {
        let _ = self.flush();
    }
}


/// Streaming png writer
///
/// This may may silently fail in the destructor so it is a good idea to call
/// `finish` or `flush` before droping. 
pub struct StreamWriter<'a, W: Write> {
    writer: deflate::write::ZlibEncoder<ChunkWriter<'a, W>>,
    prev_buf: Vec<u8>,
    curr_buf: Vec<u8>,
    index: usize,
    bpp: usize,
    filter: FilterType,
}

impl<'a, W: Write> StreamWriter<'a, W> {
    fn new(writer: &'a mut Writer<W>, buf_len: usize) -> StreamWriter<'a, W> {
        let bpp      = writer.info.bytes_per_pixel();
        let in_len   = writer.info.raw_row_length() - 1;
        let filter   = writer.info.filter;
        let prev_buf = vec![0; in_len];
        let curr_buf = vec![0; in_len];

        let compression = writer.info.compression.clone();
        let chunk_writer = ChunkWriter::new(writer, buf_len);
        let zlib = deflate::write::ZlibEncoder::new(chunk_writer, compression);

        StreamWriter {
            writer: zlib,
            index: 0,
            prev_buf,
            curr_buf,
            bpp,
            filter,
        }
    }

    pub fn finish(mut self) -> Result<()> {
        // TODO: call `writer.finish` somehow?
        self.flush()?;
        Ok(())
    }
}

impl<'a, W: Write> Write for StreamWriter<'a, W> {
    fn write(&mut self, mut buf: &[u8]) -> io::Result<usize> {
        let written = buf.read(&mut self.curr_buf[self.index..])?;
        self.index += written;
        
        if self.index >= self.curr_buf.len() {
            self.writer.write_all(&[self.filter as u8])?;
            filter(self.filter, self.bpp, &self.prev_buf, &mut self.curr_buf);
            self.writer.write_all(&self.curr_buf)?;
            mem::swap(&mut self.prev_buf, &mut self.curr_buf);
            self.index = 0;
        }

        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()?;
        if self.index > 0 {
            dbg!(self.index);
            let message = format!("wrong data size, got {} bytes too many", self.index);
            return Err(EncodingError::Format(message.into()).into());
        }
        Ok(())
    }
}

impl<'a, W: Write> Drop for StreamWriter<'a, W> {
    fn drop(&mut self) {
        let _ = self.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    extern crate rand;
    extern crate glob;

    use self::rand::Rng;
    use std::{io, cmp};
    use std::io::Write;
    use std::fs::File;

    #[test]
    fn roundtrip() {
        // More loops = more random testing, but also more test wait time
        for _ in 0..10 {
            for path in glob::glob("tests/pngsuite/*.png").unwrap().map(|r| r.unwrap()) {
                if path.file_name().unwrap().to_str().unwrap().starts_with("x") {
                    // x* files are expected to fail to decode
                    continue;
                }
                // Decode image
                let decoder = crate::Decoder::new(File::open(path).unwrap());
                let (info, mut reader) = decoder.read_info().unwrap();
                if info.line_size != 32 {
                    // TODO encoding only works with line size 32?
                    continue;
                }
                let mut buf = vec![0; info.buffer_size()];
                reader.next_frame(&mut buf).unwrap();
                // Encode decoded image
                let mut out = Vec::new();
                {
                    let mut wrapper = RandomChunkWriter {
                        rng: self::rand::thread_rng(),
                        w: &mut out
                    };

                    let mut encoder = Encoder::new(&mut wrapper, info.width, info.height).write_header().unwrap();
                    encoder.write_image_data(&buf).unwrap();
                }
                // Decode encoded decoded image
                let decoder = crate::Decoder::new(&*out);
                let (info, mut reader) = decoder.read_info().unwrap();
                let mut buf2 = vec![0; info.buffer_size()];
                reader.next_frame(&mut buf2).unwrap();
                // check if the encoded image is ok:
                assert_eq!(buf, buf2);
            }
        }
    }

    #[test]
    fn roundtrip_stream() {
        // More loops = more random testing, but also more test wait time
        for _ in 0..10 {
            for path in glob::glob("tests/pngsuite/*.png").unwrap().map(|r| r.unwrap()) {
                if path.file_name().unwrap().to_str().unwrap().starts_with("x") {
                    // x* files are expected to fail to decode
                    continue;
                }
                // Decode image
                let decoder = crate::Decoder::new(File::open(path).unwrap());
                let (info, mut reader) = decoder.read_info().unwrap();
                if info.line_size != 32 {
                    // TODO encoding only works with line size 32?
                    continue;
                }
                let mut buf = vec![0; info.buffer_size()];
                reader.next_frame(&mut buf).unwrap();
                // Encode decoded image
                let mut out = Vec::new();
                {
                    let mut wrapper = RandomChunkWriter {
                        rng: self::rand::thread_rng(),
                        w: &mut out
                    };

                    let mut encoder = Encoder::new(&mut wrapper, info.width, info.height).write_header().unwrap();
                    let mut stream_writer = encoder.stream_writer();

                    let mut outer_wrapper = RandomChunkWriter {
                        rng: self::rand::thread_rng(),
                        w: &mut stream_writer
                    };
                    
                    outer_wrapper.write_all(&buf).unwrap();
                }
                // Decode encoded decoded image
                let decoder = crate::Decoder::new(&*out);
                let (info, mut reader) = decoder.read_info().unwrap();
                let mut buf2 = vec![0; info.buffer_size()];
                reader.next_frame(&mut buf2).unwrap();
                // check if the encoded image is ok:
                assert_eq!(buf, buf2);
            }
        }
    }

    #[test]
    fn image_palette() -> Result<()> {
        use bitbit::BitWriter;
        let samples = 3;
        for bit_depth in vec![1, 2, 4, 8] {
            let path = format!("tests/pngsuite/basn3p0{}.png", bit_depth);
            let decoder = crate::Decoder::new(File::open(&path).unwrap());
            let (info, mut reader) = decoder.read_info().unwrap();
            let palette: Vec<u8> = {
                let palette = &reader.info().palette.clone().unwrap();
                palette.to_vec()
            };
            let mut buf = vec![0; info.buffer_size()];
            reader.next_frame(&mut buf).unwrap();

            let mut bit = Vec::new();
            {
                let mut bw = BitWriter::new(&mut bit);
                for i in 0..buf.len() / samples {
                    for j in 0..palette.len() / samples {
                        if buf[samples * i..samples * i + samples] ==
                            palette[samples * j..samples * j + samples]
                        {
                            bw.write_bits(j as u32, bit_depth as usize).unwrap();
                            break;
                        }
                    }
                }

                bw.pad_to_byte().unwrap();
            };

            let mut out = Vec::new();
            {
                let mut encoder = Encoder::new(&mut out, info.width, info.height);
                encoder.set(BitDepth::from_u8(bit_depth).unwrap());
                encoder.set(ColorType::Indexed);
                encoder.set_palette(palette.clone());

                let mut writer = encoder.write_header().unwrap();
                writer.write_image_data(&bit).unwrap();
            }

            // Decode encoded decoded image
            let decoder = crate::Decoder::new(&*out);
            let (info, mut reader) = decoder.read_info().unwrap();
            let mut buf2 = vec![0; info.buffer_size()];
            reader.next_frame(&mut buf2).unwrap();
            // check if the encoded image is ok:
            assert_eq!(buf, buf2);
        }
        Ok(())
    }

    #[test]
    fn expect_error_on_wrong_image_len() -> Result<()> {
        use std::io::Cursor;

        let width = 10;
        let height = 10;

        let output = vec![0u8; 1024];
        let writer = Cursor::new(output);
        let mut encoder = Encoder::new(writer, width as u32, height as u32);
        encoder.set(BitDepth::Eight);
        encoder.set(ColorType::RGB);
        let mut png_writer = encoder.write_header()?;

        let correct_image_size = width * height * 3;
        let image = vec![0u8; correct_image_size + 1];
        let result = png_writer.write_image_data(image.as_ref());
        assert!(result.is_err());

        Ok(())
    }

    /// A Writer that only writes a few bytes at a time
    struct RandomChunkWriter<'a, R: Rng, W: Write + 'a> {
        rng: R,
        w: &'a mut W
    }

    impl<'a, R: Rng, W: Write + 'a> Write for RandomChunkWriter<'a, R, W> {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            // choose a random length to write
            let len = cmp::min(self.rng.gen_range(1, 50), buf.len());

            self.w.write(&buf[0..len])
        }

        fn flush(&mut self) -> io::Result<()> {
            self.w.flush()
        }
    }

}
