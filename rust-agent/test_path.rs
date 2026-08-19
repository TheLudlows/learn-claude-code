use std::path::PathBuf;
use path_clean::PathClean;

fn main() {
    let workdir = std::env::current_dir().unwrap().canonicalize().unwrap();
    let directory = workdir.clone();
    
    println!("workdir: {:?}", workdir);
    println!("directory: {:?}", directory);
    
    let normalized = workdir.join(&directory).clean();
    println!("normalized: {:?}", normalized);
    println!("starts_with: {}", normalized.starts_with(&workdir));
    println!("equals: {}", normalized == workdir);
}
