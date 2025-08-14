use rand::Rng;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Prompt {
    pub text: String,
    pub quality: f64,
    pub diversity: f64,
}

impl Prompt {
    pub fn new(text: String) -> Self {
        Prompt {
            text,
            quality: 0.0,
            diversity: 0.0,
        }
    }
}

pub struct GepaOptimizer {
    population_size: usize,
    generations: usize,
    mutation_rate: f64,
}

impl GepaOptimizer {
    pub fn new(population_size: usize, generations: usize, mutation_rate: f64) -> Self {
        GepaOptimizer {
            population_size,
            generations,
            mutation_rate,
        }
    }

    pub fn optimize(&self, initial_prompts: &[String]) -> Vec<Prompt> {
        let mut population: Vec<Prompt> = initial_prompts
            .iter()
            .map(|p| Prompt::new(p.clone()))
            .collect();

        for _ in 0..self.generations {
            // Evaluate the population (in a real scenario, this would involve an external model)
            for prompt in &mut population {
                prompt.quality = self.evaluate_quality(&prompt.text);
                prompt.diversity = self.evaluate_diversity(&prompt.text, &population);
            }

            // Selection
            let parents = self.selection(&population);

            // Crossover and Mutation
            let mut offspring = Vec::new();
            for i in (0..parents.len()).step_by(2) {
                if i + 1 < parents.len() {
                    let (child1, child2) = self.crossover(&parents[i], &parents[i + 1]);
                    offspring.push(self.mutate(child1));
                    offspring.push(self.mutate(child2));
                }
            }
            population.extend(offspring);

            // Survivor selection (keep the best)
            population.sort_by(|a, b| b.quality.partial_cmp(&a.quality).unwrap());
            population.truncate(self.population_size);
        }

        self.pareto_front(&population)
    }

    fn evaluate_quality(&self, text: &str) -> f64 {
        // Placeholder for quality evaluation.
        // In a real implementation, this would call an LLM or other evaluation function.
        text.len() as f64
    }

    fn evaluate_diversity(&self, text: &str, population: &[Prompt]) -> f64 {
        // Placeholder for diversity evaluation.
        // A simple diversity metric could be the average Levenshtein distance to other prompts.
        population
            .iter()
            .map(|p| levenshtein::levenshtein(text, &p.text) as f64)
            .sum::<f64>()
            / population.len() as f64
    }

    fn selection<'a>(&self, population: &'a [Prompt]) -> Vec<&'a Prompt> {
        // Tournament selection
        let mut parents = Vec::new();
        let mut rng = rand::thread_rng();
        for _ in 0..self.population_size {
            let i = rng.gen_range(0..population.len());
            let j = rng.gen_range(0..population.len());
            if population[i].quality > population[j].quality {
                parents.push(&population[i]);
            } else {
                parents.push(&population[j]);
            }
        }
        parents
    }

    fn crossover(&self, parent1: &Prompt, parent2: &Prompt) -> (Prompt, Prompt) {
        let mut rng = rand::thread_rng();
        let crossover_point = rng.gen_range(0..parent1.text.len().min(parent2.text.len()));
        let child1_text = format!(
            "{}{}",
            &parent1.text[..crossover_point],
            &parent2.text[crossover_point..]
        );
        let child2_text = format!(
            "{}{}",
            &parent2.text[..crossover_point],
            &parent1.text[crossover_point..]
        );
        (Prompt::new(child1_text), Prompt::new(child2_text))
    }

    fn mutate(&self, mut prompt: Prompt) -> Prompt {
        let mut rng = rand::thread_rng();
        if rng.gen_bool(self.mutation_rate) {
            let mut chars: Vec<char> = prompt.text.chars().collect();
            if !chars.is_empty() {
                let mutation_point = rng.gen_range(0..chars.len());
                // Simple mutation: swap two characters
                let swap_point = rng.gen_range(0..chars.len());
                chars.swap(mutation_point, swap_point);
                prompt.text = chars.into_iter().collect();
            }
        }
        prompt
    }

    fn pareto_front(&self, population: &[Prompt]) -> Vec<Prompt> {
        let mut pareto = Vec::new();
        for p1 in population {
            let mut is_dominated = false;
            for p2 in population {
                if p1.quality < p2.quality && p1.diversity < p2.diversity {
                    is_dominated = true;
                    break;
                }
            }
            if !is_dominated {
                pareto.push(p1.clone());
            }
        }
        pareto
    }
}
