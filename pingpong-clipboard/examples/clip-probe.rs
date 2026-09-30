//! What this end's clipboard holds, as clipboard sharing sees it.
fn main() {
    let mut cb = arboard::Clipboard::new().expect("clipboard");
    println!("file_list: {:?}", cb.get().file_list());
    println!("text: {:?}", cb.get_text().map(|t| t.len()));
    println!("image: {:?}", cb.get_image().map(|i| (i.width, i.height)));
    let mut b = pingpong_clipboard::Board::open();
    println!("stamp: {:?} concealed: {}", b.stamp(), b.concealed());
    println!("why_not: {}", b.why_not());
    println!("read: {:?}", b.read(true).map(|items| items.len()));
}
