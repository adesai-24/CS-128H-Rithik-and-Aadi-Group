pub struct User {
    pub name: Option<String>,
    pub age: Option<u32>,
    pub id_num: String,
}

// simple implementation of User struct
impl User {
    pub fn new(name: Option<String>, age: Option<u32>, id_num: String) -> User {
        User {
            name,
            age,
            id_num,
        }
    }
    pub fn get_name(&self) -> &str {
        &self.name
    }
    pub fn get_age(&self) -> u32 {
        self.age
    }
    pub fn get_id_num(&self) -> &str {
        &self.id_num
    }
}