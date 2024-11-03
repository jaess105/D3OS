#![no_std]

extern crate alloc;

use alloc::string::String;
use persistent::{create_persistent_pool, release_persistent_pool};
use terminal::read::read;
use terminal::{print, println};
#[allow(unused_imports)]
use runtime::*;

#[unsafe(no_mangle)]
pub fn main() {
    print!("Choose operation (c)reate or (r)elease pool: ");

    let operation = loop {
        if let Some(ch) = read() {
            match ch {
                'c' | 'C' => {
                    println!("\nCreate mode");
                    break true;
                }
                'r' | 'R' => {
                    println!("\nRelease mode");
                    break false;
                }
                '\n' => continue,
                _ => {
                    println!("\nInvalid input! Please enter 'c' or 'r'");
                    continue;
                },
            }
        }
    };

    let mut input = String::new();
    print!("Enter pool name: ");

    loop {
        match read() {
            Some(ch) => {
                match ch {
                    '\n' => break,
                    _ => {
                        input.push(ch);
                        //print!("{}", ch); // Echo character
                    }
                }
            }
            None => (),
        }
    }
    print!("\n");

    if operation {
        match create_persistent_pool(&input) {
            Ok(_) => println!("Successfully created/accessed pool '{}'", input),
            Err(e) => println!("Error: {}", e),
        }
    } else {
        match release_persistent_pool(&input) {
            Ok(_) => println!("Successfully released pool '{}'", input),
            Err(e) => println!("Error: {}", e),
        }
    }
}
