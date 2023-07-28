// An implementation of LLaMA https://github.com/facebookresearch/llama
//
// This is based on nanoGPT in a similar way to:
// https://github.com/Lightning-AI/lit-llama/blob/main/lit_llama/model.py
//
// The tokenizer config can be retrieved from:
// https://huggingface.co/hf-internal-testing/llama-tokenizer/raw/main/tokenizer.json
//
// In order to convert the llama weights to a .npz file, run:
// python examples/llama/convert_checkpoint.py ..../LLaMA/7B/consolidated.00.pth

#[cfg(feature = "mkl")]
extern crate intel_mkl_src;

use anyhow::{Error as E, Result};
use clap::Parser;

use candle::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::generation::LogitsProcessor;
use cudarc::driver::safe::CudaDevice;
use cudarc::nccl::safe::{Comm, Id};
use hf_hub::{api::sync::Api, Repo, RepoType};
use std::io::Write;
use std::rc::Rc;

mod model;
use model::{Config, Llama};

const MAX_SEQ_LEN: usize = 4096;
const DEFAULT_PROMPT: &str = r"
EDWARD:
I wonder how our princely father 'scaped,
Or whether he be 'scaped away or no
From Clifford's and Northumberland's pursuit:
Had he been ta'en, we should have heard the news;
Had he been slain, we should have heard the news;
Or had he 'scaped, methinks we should have heard
The happy tidings of his good escape.
How fares my brother? why is he so sad?

RICHARD:
I cannot joy, until I be resolved
Where our right valiant father is become.
I saw him in the battle range about;
And watch'd him how he singled Clifford forth.
Methought he bore him in the thickest troop
As doth a lion in a herd of neat;
Or as a bear, encompass'd round with dogs,
Who having pinch'd a few and made them cry,
The rest stand all aloof, and bark at him.
So fared our father with his enemies;
So fled his enemies my warlike father:
Methinks, 'tis prize enough to be his son.
See how the morning opes her golden gates,
And takes her farewell of the glorious sun!
How well resembles it the prime of youth,
Trimm'd like a younker prancing to his love!

EDWARD:
Dazzle mine eyes, or do I see three suns?

RICHARD:
Three glorious suns, each one a perfect sun;
Not separated with the racking clouds,
But sever'd in a pale clear-shining sky.
See, see! they join, embrace, and seem to kiss,
As if they vow'd some league inviolable:
Now are they but one lamp, one light, one sun.
In this the heaven figures some event.

EDWARD:
'Tis wondrous strange, the like yet never heard of.
I think it cites us, brother, to the field,
That we, the sons of brave Plantagenet,
Each one already blazing by our meeds,
Should notwithstanding join our lights together
And over-shine the earth as this the world.
Whate'er it bodes, henceforward will I bear
Upon my target three fair-shining suns.
";

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Run on CPU rather than on GPU.
    #[arg(long)]
    cpu: bool,

    #[arg(long)]
    num_shards: usize,

    #[arg(long)]
    rank: Option<usize>,

    /// The temperature used to generate samples.
    #[arg(long)]
    temperature: Option<f64>,

    /// The seed to use when generating random samples.
    #[arg(long, default_value_t = 299792458)]
    seed: u64,

    /// The length of the sample to generate (in tokens).
    #[arg(long, default_value_t = 100)]
    sample_len: usize,

    /// Disable the key-value cache.
    #[arg(long)]
    no_kv_cache: bool,

    /// The initial prompt.
    #[arg(long)]
    prompt: Option<String>,

    /// Use f32 computations rather than f16.
    #[arg(long)]
    use_f32: bool,

    #[arg(long)]
    model_id: Option<String>,

    #[arg(long)]
    v2: bool,
}

fn main() -> Result<()> {
    use tokenizers::Tokenizer;

    let args = Args::parse();

    let config = Config::config_7b();
    let dtype = if args.use_f32 { DType::F32 } else { DType::F16 };

    let api = Api::new()?;

    let model_id = args.model_id.unwrap_or_else(|| {
        if args.v2 {
            "meta-llama/Llama-2-7b-hf".to_string()
        } else {
            "Narsil/amall-7b".to_string()
        }
    });
    println!("loading the model weights from {model_id}");
    let repo = Repo::new(model_id, RepoType::Model);
    let tokenizer_filename = api.get(&repo, "tokenizer.json")?;
    let mut filenames = vec![];
    for rfilename in [
        "model-00001-of-00002.safetensors",
        "model-00002-of-00002.safetensors",
    ] {
        let filename = api.get(&repo, rfilename)?;
        filenames.push(filename);
    }

    if args.rank.is_none() {
        let children: Vec<_> = (0..args.num_shards)
            .map(|rank| {
                let mut args: std::collections::VecDeque<_> = std::env::args().collect();
                args.push_back("--rank".to_string());
                args.push_back(format!("{rank}"));
                let name = args.pop_front().unwrap();
                std::process::Command::new(name).args(args).spawn().unwrap()
            })
            .collect();
        for mut child in children {
            child.wait().unwrap();
        }
        return Ok(());
    }

    let i = args.rank.unwrap();
    let num_shards = args.num_shards;
    let rank = i;
    // Primitive IPC
    let id = if rank == 0 {
        let id = Id::new().unwrap();
        std::fs::File::create("nccl_id.txt.tmp")?
            .write_all(&id.internal().iter().map(|&i| i as u8).collect::<Vec<_>>())
            .unwrap();
        std::fs::rename("nccl_id.txt.tmp", "nccl_id.txt")?;
        id
    } else {
        let path = std::path::PathBuf::from("nccl_id.txt");
        while !path.exists() {
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
        let data = std::fs::read("nccl_id.txt")?;
        let internal: [i8; 128] = data
            .into_iter()
            .map(|i| i as i8)
            .collect::<Vec<_>>()
            .try_into()
            .unwrap();
        let id: Id = Id::uninit(internal);
        id
    };
    let device = CudaDevice::new(i)?;
    let comm = Rc::new(Comm::from_rank(device, i, num_shards, id).unwrap());
    if rank == 0 {
        std::fs::remove_file("nccl_id.txt")?;
    }
    println!("Rank {rank:?} spawned");

    let device = Device::new_cuda(i)?;
    let cache = model::Cache::new(!args.no_kv_cache, &config, &device)?;

    println!("building the model");
    let handles = filenames
        .iter()
        .map(|f| Ok(unsafe { candle::safetensors::MmapedFile::new(f.as_path())? }))
        .collect::<Result<Vec<_>>>()?;
    let tensors: Vec<_> = handles
        .iter()
        .map(|h| Ok(h.deserialize()?))
        .collect::<Result<Vec<_>>>()?;

    let vb = VarBuilder::from_safetensors(tensors, dtype, &device);
    let llama = Llama::load(vb, &cache, &config, comm)?;
    let tokenizer = Tokenizer::from_file(tokenizer_filename).map_err(E::msg)?;

    let prompt = args.prompt.as_ref().map_or(DEFAULT_PROMPT, |p| p.as_str());
    let mut tokens = tokenizer
        .encode(prompt, true)
        .map_err(E::msg)?
        .get_ids()
        .to_vec();

    println!("starting the inference loop");
    let mut logits_processor = LogitsProcessor::new(args.seed, args.temperature);
    let mut new_tokens = vec![];
    let start_gen = std::time::Instant::now();
    let mut index_pos = 0;
    for index in 0..args.sample_len {
        let start_gen = std::time::Instant::now();
        let context_size = if cache.use_kv_cache && index > 0 {
            1
        } else {
            tokens.len()
        };
        let ctxt = &tokens[tokens.len().saturating_sub(context_size)..];
        let input = Tensor::new(ctxt, &device)?.unsqueeze(0)?;
        let logits = llama.forward(&input, index_pos)?;
        let logits = logits.squeeze(0)?;
        index_pos += ctxt.len();

        let next_token = logits_processor.sample(&logits)?;
        tokens.push(next_token);
        new_tokens.push(next_token);
        if rank == 0 {
            println!("> {:?}", start_gen.elapsed());
            println!(
                "{} token: {} '{}'",
                index + 1,
                next_token,
                tokenizer.decode(vec![next_token], true).map_err(E::msg)?
            );
        }
    }
    let dt = start_gen.elapsed();
    if rank == 0 {
        println!(
            "{} tokens generated ({} token/s)\n----\n{}\n----",
            args.sample_len,
            args.sample_len as f64 / dt.as_secs_f64(),
            tokenizer.decode(new_tokens, true).map_err(E::msg)?
        );
    }
    Ok(())
}