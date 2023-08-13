pub type Column = usize;
pub type Row = usize;

pub fn convert_row_column_to_letter(mut row: Row, mut column: Column) -> String {
    row += 1;
    column += 1;
    let letters = ('A'..='Z').collect::<Vec<char>>();

    let mut res = String::new();
    if column < 26 {
        res.push(letters[column - 1]);
        res += &row.to_string();
        return res;
    }

    while column / 26 != 0 {
        let letter = column / 26;
        res.push(letters[letter - 1]);
        column %= 26;
    }

    if column > 0 {
        res.push(letters[column - 1]);
    }

    res += &row.to_string();

    res
}
