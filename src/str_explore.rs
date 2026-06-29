// 用 rustc str_explore.rs && ./str_explore 来编译运行
// 每个 exercise 先去掉注释，猜结果，再编译看编译器怎么说

fn main() {
    // === Exercise 1: 谁能改，谁不能改？ ===
    let a: &str = "hello";
    let mut b: String = String::from("hello");

    // 取消下面两行的注释，分别编译，看哪个报错：
    // a.push_str(" world");
    // b.push_str(" world");

    // === Exercise 2: 赋值和所有权 ===
    let s1: String = String::from("rust");
    let s2 = s1;

    // 取消下面这行，编译：
    // println!("s1 = {}", s1);

    // 换成 &str 试试：
    let s3: &str = "rust";
    let s4 = s3;
    // println!("s3 = {}", s3);

    // === Exercise 3: 函数参数 ===
    // 先取消 greet_str 的调用，再取消 greet_string 的调用
    // 观察哪些组合能编译，哪些不能

    let owned: String = String::from("thom");
    let borrowed: &str = "thom";

    // greet_str(borrowed);
    // greet_str(owned);
    // greet_string(borrowed);
        greet_string(owned);

    // === Exercise 4: 函数返回值 ===
    // 取消注释，编译，读错误信息：
    // let result = make_greeting("thom");

    // === Exercise 5: 大小 ===
    println!("size of &str:   {} bytes", std::mem::size_of::<&str>());
    println!("size of String: {} bytes", std::mem::size_of::<String>());
    // 想想为什么是这些数字
}

fn greet_str(name: &str) {
    println!("Hello, {}!", name);
}

fn greet_string(name: String) {
    println!("Hello, {}!", name);
}

// // 取消注释这个函数，看编译器说什么：
// fn make_greeting(name: &str) -> &str {
//     let result = format!("Hello, {}!", name);
//     &result.to_string()
// }
