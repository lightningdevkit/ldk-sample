
use lightning::ln::features;
fn main() {					
	//change in python

	let node_features = features::NodeFeatures::known();
	println!("{}",node_features);
}

