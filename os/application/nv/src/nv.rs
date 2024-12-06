#![no_std]

extern crate alloc;

use alloc::string::String;
use persistent::{create_persistent_pool, perform_transaction, release_persistent_pool};
use terminal::read::read;
use terminal::{print, println};
#[allow(unused_imports)]
use runtime::*;
use terminal::write::print;

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

    // if operation {
    //     match create_persistent_pool(&input) {
    //         Ok(_) => println!("Successfully created/accessed pool '{}'", input),
    //         Err(e) => println!("Error: {}", e),
    //     }
    // } else {
    //     match release_persistent_pool(&input) {
    //         Ok(_) => println!("Successfully released pool '{}'", input),
    //         Err(e) => println!("Error: {}", e),
    //     }
    // }

    //Create/access pool
    match create_persistent_pool(&input) {
        Ok(_) => {
            // Get data to store
            let mut data = String::new();
            print!("Enter data to store: ");

            loop {
                match read() {
                    Some(ch) => {
                        match ch {
                            '\n' => break,
                            _ => {
                                data.push(ch);
                            },
                        }
                    },
                    None => (),
                }
            }
            print!("\n");
            print!("Storing data in pool as : '{:#?}'\n", data.as_bytes());
            match perform_transaction(&input, data.as_bytes(), 123) {
                Ok(_) => println!("Successfully stored data in pool"),
                Err(e) => println!("Transaction failed: {}", e),
            }
        },
        Err(e) => println!("Failed to create/access pool: {}", e),
    }

}
