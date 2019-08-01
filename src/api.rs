use crate::{helpers, memory::*};
use rayon::prelude::*;
use rand::prelude::*;
use packed_simd::{Simd, SimdArray};

pub type InitDoneCallbackFn<'a, T> = &'a dyn Fn(&KMeansState<T>);
pub type IterationDoneCallbackFn<'a, T> = &'a dyn Fn(&KMeansState<T>, usize, T);

/// This is a structure holding various callbacks, that can be set to get status information from
/// a running k-means calculation.
#[derive(Clone)]
pub struct KMeansEvt<'a, T: Primitive> {
    /// Callback that is called, when the initialization phase finished
    /// ## Arguments
    /// - **state**: Current [`KMeansState`] after the initialization
    pub(crate) init_done: InitDoneCallbackFn<'a, T>,
    /// Callback that is called after each iteration
    /// ## Arguments
    /// - **state**: Current[`KMeansState`] after the iteration
    /// - **iteration_id**: Number of the current iteration
    /// - **distsum**: New distance sum (**state** contains the distsum from the previous iteration)
    pub(crate) iteration_done: IterationDoneCallbackFn<'a, T>
}
impl<'a, T: Primitive> KMeansEvt<'a, T> {
    pub fn empty() -> Self {
        Self {
            init_done: &|_| {},
            iteration_done: &|_,_,_| {}
        }
    }
    pub fn build() -> KMeansEvtBuilder<'a, T> {
        KMeansEvtBuilder { evt: KMeansEvt::empty() }
    }
}
impl<'a, T: Primitive> std::fmt::Debug for KMeansEvt<'a, T> {
    fn fmt(&self, _: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { Ok(()) }
}

pub struct KMeansEvtBuilder<'a, T: Primitive> {
    evt: KMeansEvt<'a, T>
}
impl<'a, T: Primitive> KMeansEvtBuilder<'a, T> {
    pub fn init_done(mut self, init_done: InitDoneCallbackFn<'a, T>) -> Self {
        self.evt.init_done = init_done; self
    }
    pub fn iteration_done(mut self, iteration_done: IterationDoneCallbackFn<'a, T>) -> Self {
        self.evt.iteration_done = iteration_done; self
    }
    pub fn build(self) -> KMeansEvt<'a, T> { self.evt }
}


/// This is the internally used data-structure, storing the current state during calculation, as
/// well as the final result, as returned by the API.
/// All mutations are done in this structure, making [`KMeans`] immutable, and therefore allowing
/// it to be used in parallel, without having to duplicate the input-data.
/// 
/// ## Generics
/// - **T**: Underlying primitive type that was used for the calculation
/// 
/// ## Fields
/// - **k**: The amount of clusters that were requested when calculating this k-means result
/// - **distsum**: The total sum of (squared) distances from all samples to their respective centroids
/// - **centroids**: Calculated cluster centers [row-major] = [<centroid0>,<centroid1>,<centroid2>,...]
/// - **centroid_frequency**: Amount of samples in each centroid
/// - **assignments**: Vector mapping each sample to its respective nearest cluster
/// - **centroid_distances**: Vector containing each sample's (squared) distance to its centroid
#[derive(Clone, Debug)]
pub struct KMeansState<'a, T: Primitive> {
    pub k: usize,
    pub distsum: T,
    pub centroids: Vec<T>,
    pub centroid_frequency: Vec<usize>,
    pub assignments: Vec<usize>,
    pub centroid_distances: Vec<T>,

    pub(crate) sample_dims: usize,
    pub(crate) evt: KMeansEvt<'a, T>
}
impl<'a, T: Primitive> KMeansState<'a, T> {
    pub(crate) fn new(sample_cnt: usize, sample_dims: usize, k: usize, evt: KMeansEvt<'a, T>) -> Self {
        Self {
            k,
            distsum: T::zero(),
            centroids: AlignedFloatVec::new(sample_dims * k),
            centroid_frequency: vec![0usize;k],
            assignments: vec![0usize;sample_cnt],
            centroid_distances: vec![T::infinity();sample_cnt],
            sample_dims,
            evt
        }
    }
    pub(crate) fn set_centroid_from_iter(&mut self, idx: usize, src: impl Iterator<Item = T>) {
        self.centroids.iter_mut().skip(self.sample_dims * idx).take(self.sample_dims)
                .zip(src)
                .for_each(|(c,s)| *c = s);
    }

    pub(crate) fn remove_padding(mut self, sample_dims: usize) -> Self {
        if self.sample_dims != sample_dims { // Datastructure was padded -> undo
            self.centroids = self.centroids.chunks_exact(self.sample_dims)
                .map(|chunk| chunk.iter().cloned().take(sample_dims)).flatten().collect();
        }
        self
    }
}




/// Entrypoint of this crate's API-Surface.
/// 
/// Create an instance of this struct, giving the samples you want to operate on. The primitive type
/// of the passed samples array will be the type used internaly for all calculations, as well as the result
/// as stored in the returned [`KMeansState`] structure.
/// 
/// ## Supported variants
/// - k-Means clustering [`KMeans::kmeans`]
/// - **\[TODO\]** Mini-Batch k-Means clustering
/// 
/// ## Supported initialization methods
/// - K-Mean++ [`KMeans::init_kmeanplusplus`]
pub struct KMeans<T> where T: Primitive, [T;LANES]: SimdArray, Simd<[T;LANES]>: SimdWrapper<T> {
    pub(crate) sample_cnt: usize,
    pub(crate) sample_dims: usize,
    pub(crate) p_sample_dims: usize,
    pub(crate) p_samples: Vec<T>
}
impl<T> KMeans<T> where T: Primitive, [T;LANES]: SimdArray, Simd<[T;LANES]>: SimdWrapper<T> {
    /// Create a new instance of the [`KMeans`] structure.
    /// 
    /// ## Arguments
    /// - **samples**: Vector of samples [row-major] = [<sample0>,<sample1>,<sample2>,...]
    /// - **sample_cnt**: Amount of samples, contained in the passed **samples** vector
    /// - **sample_dims**: Amount of dimensions each sample from the **sample** vector has
    pub fn new(samples: Vec<T>, sample_cnt: usize, sample_dims: usize) -> Self {
        assert!(samples.len() == sample_cnt * sample_dims);
        let p_sample_dims = helpers::multiple_roundup(sample_dims, LANES);
       
        // Recopy into new, properly aligned + padded buffer
        let mut aligned_samples = AlignedFloatVec::new(sample_cnt * p_sample_dims);
        if p_sample_dims == sample_dims {
            aligned_samples.copy_from_slice(&samples);
        } else {
            for s in 0..sample_cnt {
                for d in 0..sample_dims {
                    aligned_samples[s * p_sample_dims + d] = samples[s * sample_dims + d];
                }
            }
        };

        Self {
            sample_cnt: sample_cnt,
            sample_dims: sample_dims,
            p_sample_dims,
            p_samples: aligned_samples
        }
    }



    pub(crate) fn update_cluster_assignments(&self, state: &mut KMeansState<T>, limit_k: Option<usize>) {
        let centroids = &state.centroids;
        let k = limit_k.unwrap_or(state.k);

		// TODO: Switch to par_chunks_mut, when that is merged in rayon (https://github.com/rayon-rs/rayon/pull/629).
		// par_chunks() works, because sample-dimensions are manually padded, so that there is no remainder
        self.p_samples.par_chunks(self.p_sample_dims)
            .zip(state.assignments.par_iter_mut())
            .zip(state.centroid_distances.par_iter_mut())
            .for_each(|((s, assignment), centroid_dist)| {
                let (best_idx, best_dist) = centroids.chunks_exact(self.p_sample_dims).take(k)
                    .map(|c| {
                        s.chunks_exact(LANES).map(|i| unsafe { Simd::<[T;LANES]>::from_slice_aligned_unchecked(i) })
                            .zip(c.chunks_exact(LANES).map(|i| unsafe { Simd::<[T;LANES]>::from_slice_aligned_unchecked(i) }))
                                .map(|(sp,cp)| sp - cp)         // <sample> - <centroid>
                                .map(|v| v * v)                 // <vec_components> ^2
                                .sum::<Simd::<[T;LANES]>>()     // sum(<vec_components>^2)
                                .sum()
                    }).enumerate()
                    .min_by(|(_,d0), (_,d1)| d0.partial_cmp(d1).unwrap()).unwrap();
                *assignment = best_idx;
                *centroid_dist = best_dist;
            });
    }

    pub(crate) fn update_cluster_frequencies(&self, assignments: &[usize], centroid_frequency: &mut[usize]) -> usize {
        centroid_frequency.iter_mut().for_each(|v| *v = 0);
        let mut used_centroids_cnt = 0;
        assignments.iter().cloned()
            .for_each(|centroid_id| {
                if centroid_frequency[centroid_id] == 0 {
                    used_centroids_cnt += 1; // Count the amount of centroids with more than 0 samples
                }
                centroid_frequency[centroid_id] += 1;
            });
        used_centroids_cnt
    }



    /// Normal K-Means algorithm implementation. This is the same algorithm as implemented in Matlab (one-phase).
    /// (see: https://uk.mathworks.com/help/stats/kmeans.html#bueq7aj-5    Section: More About)
    /// 
    /// ## Arguments
    /// - **k**: Amount of clusters to search for
    /// - **max_iter**: Limit the maximum amount of iterations (just pass a high number for infinite)
    /// - **init**: Initialization-Method to use for the initialization of the **k** centroids
    /// - **rnd**: Random number generator to use (Pass a seeded one, if you want reproducible results)
    /// - **evt**: Optional [`KMeansEvt`] instance, containing callbacks that notify about status of the calculation.
    /// 
    /// ## Returns
    /// Instance of [`KMeansState`], containing the final state (result).
    /// 
    /// ## Example
    /// ```rust
    /// use kmeans::*;
    /// fn main() {
    ///     let (sample_cnt, sample_dims, k, max_iter) = (20000, 200, 4, 100);
    /// 
    ///     // Generate some random data
    ///     let mut samples = vec![0.0f64;sample_cnt * sample_dims];
    ///     samples.iter_mut().for_each(|v| *v = rand::random());
    /// 
    ///     // Calculate kmeans, using kmean++ as initialization-method
    ///     let kmean = KMeans::new(samples, sample_cnt, sample_dims);
    ///     let result = kmean.kmeans_lloyd(k, max_iter, KMeans::init_kmeanplusplus, &mut rand::thread_rng(), None);
    /// 
    ///     println!("Centroids: {:?}", result.centroids);
    ///     println!("Cluster-Assignments: {:?}", result.assignments);
    ///     println!("Error: {}", result.distsum);
    /// }
    /// ```
    pub fn kmeans_lloyd<'a, 'b, F>(&self, k: usize, max_iter: usize, init: F, rnd: &'a mut dyn RngCore, evt: Option<KMeansEvt<'b, T>>) -> KMeansState<'b, T>
                where for<'c> F: FnOnce(&KMeans<T>, &mut KMeansState<T>, &'c mut dyn RngCore) {
        crate::variants::Lloyd::calculate(&self, k, max_iter, init, rnd, evt.unwrap_or(KMeansEvt::empty()))
    }

    /// Mini-Batch k-Means implementation.
    /// (see: https://dl.acm.org/citation.cfm?id=1772862)
    /// 
    /// ## Arguments
    /// - **batch_size**: Amount of samples to use per iteration (higher -> better approximation but slower)
    /// - **k**: Amount of clusters to search for
    /// - **max_iter**: Limit the maximum amount of iterations (just pass a high number for infinite)
    /// - **init**: Initialization-Method to use for the initialization of the **k** centroids
    /// - **rnd**: Random number generator to use (Pass a seeded one, if you want reproducible results)
    /// - **evt**: Optional [`KMeansEvt`] instance, containing callbacks that notify about status of the calculation.
    /// 
    /// ## Returns
    /// Instance of [`KMeansState`], containing the final state (result).
    /// 
    /// ## Example
    /// ```rust
    /// use kmeans::*;
    /// fn main() {
    ///     let (sample_cnt, sample_dims, k, max_iter) = (20000, 200, 4, 100);
    ///
    ///     // Generate some random data
    ///     let mut samples = vec![0.0f64;sample_cnt * sample_dims];
    ///     samples.iter_mut().for_each(|v| *v = rand::random());
    ///
    ///     // Calculate kmeans, using kmean++ as initialization-method
    ///     let kmean = KMeans::new(samples, sample_cnt, sample_dims);
    ///     let result = kmean.kmeans_minibatch(4, k, max_iter, KMeans::init_random_sample, &mut rand::thread_rng(), None);
    ///
    ///     println!("Centroids: {:?}", result.centroids);
    ///     println!("Cluster-Assignments: {:?}", result.assignments);
    ///     println!("Error: {}", result.distsum);
    /// }
    /// ```
    pub fn kmeans_minibatch<'a, 'b, F>(&self, batch_size: usize, k: usize, max_iter: usize, init: F, rnd: &'a mut dyn RngCore, evt: Option<KMeansEvt<'b, T>>) -> KMeansState<'b, T>
            where for<'c> F: FnOnce(&KMeans<T>, &mut KMeansState<T>, &'c mut dyn RngCore) {
        crate::variants::Minibatch::calculate(&self, batch_size, k, max_iter, init, rnd, evt.unwrap_or(KMeansEvt::empty()))
    }

    /// K-Means++ initialization method, as implemented in Matlab
    /// 
    /// ## Description
    /// This initialization method starts by selecting one sample as first centroid.
    /// Proceeding from there, the method iteratively selects one new centroid (per iteration) by calculating
    /// each sample's probability of "being a centroid". This probability is bigger, the farther away a sample
    /// is from its centroid. Then, one sample is randomly selected, while taking their probability of being
    /// the next centroid into account. This leads to a tendency of selecting centroids, that are far away from
    /// their currently assigned cluster's centroid.
    /// (see: https://uk.mathworks.com/help/stats/kmeans.html#bueq7aj-5    Section: More About)
    /// 
    /// ## Note
    /// This method is not meant for direct invocation. Pass a reference to it, to an instance-method of [`KMeans`].
    pub fn init_kmeanplusplus<'a>(kmean: &KMeans<T>, state: &mut KMeansState<T>, rnd: &'a mut dyn RngCore) {
        crate::inits::kmeanplusplus::calculate(kmean, state, rnd);
    }

    /// Random-Parition initialization method
    /// 
    /// ## Description
    /// This initialization method randomly partitions the samples into k partitions, and then calculates these partion's means.
    /// These means are then used as initial clusters.
    /// 
    pub fn init_random_partition<'a>(kmean: &KMeans<T>, state: &mut KMeansState<T>, rnd: &'a mut dyn RngCore) {
        crate::inits::randompartition::calculate(kmean, state, rnd);
    }

    /// Random sample initialization method (a.k.a. Forgy)
    /// 
    /// ## Description
    /// This initialization method randomly selects k centroids from the samples as initial centroids.
    /// 
    /// ## Note
    /// This method is not meant for direct invocation. Pass a reference to it, to an instance-method of [`KMeans`].
    pub fn init_random_sample<'a>(kmean: &KMeans<T>, state: &mut KMeansState<T>, rnd: &'a mut dyn RngCore) {
        crate::inits::randomsample::calculate(kmean, state, rnd);
    }

}


#[cfg(test)]
mod tests {
    use super::*;
    use test::Bencher;

    #[test]
    fn padding_and_cluster_assignments() {
        calculate_cluster_assignments_multiplex(1);
        calculate_cluster_assignments_multiplex(2);
        calculate_cluster_assignments_multiplex(3);
        calculate_cluster_assignments_multiplex(97);
        calculate_cluster_assignments_multiplex(98);
        calculate_cluster_assignments_multiplex(99);
        calculate_cluster_assignments_multiplex(100);
    }

    fn calculate_cluster_assignments_multiplex(sample_dims: usize) {
        calculate_cluster_assignments::<f64>(sample_dims, 1e-10f64);
        calculate_cluster_assignments::<f32>(sample_dims, 1e-5f32);
    }

    fn calculate_cluster_assignments<T: Primitive>(sample_dims: usize, max_diff: T) where [T;LANES] : SimdArray, Simd<[T;LANES]>: SimdWrapper<T> {
        let sample_cnt = 1000;
        let k = 5;

        let mut samples = vec![T::zero();sample_cnt * sample_dims];
        samples.iter_mut().for_each(|i| *i = thread_rng().gen_range(T::zero(), T::one()));

        let kmean = KMeans::new(samples, sample_cnt, sample_dims);
        
        let mut state = KMeansState::new(kmean.sample_cnt, kmean.p_sample_dims, k, KMeansEvt::empty());
        state.centroids.iter_mut()
            .zip(kmean.p_samples.iter())
            .for_each(|(c,s)| *c = *s);

        // calculate distances using method that (hopefully) works.
        let mut should_assignments = state.assignments.clone();
        let mut should_centroid_distances = state.centroid_distances.clone();
        kmean.p_samples.chunks_exact(kmean.p_sample_dims)
            .zip(should_assignments.iter_mut())
            .zip(should_centroid_distances.iter_mut())
            .for_each(|((s, assignment), centroid_dist)| {
                let (best_idx, best_dist) = state.centroids
                    .chunks_exact(kmean.p_sample_dims)
                    .map(|c| {
                        s.iter().cloned().zip(c.iter().cloned())
                            .map(|(sv,cv)| sv - cv)
                            .map(|v| v * v)
                            .sum::<T>()
                    })
                    .enumerate()
                    .min_by(|(_,d0), (_,d1)| d0.partial_cmp(d1).unwrap())
                    .unwrap();
                *assignment = best_idx;
                *centroid_dist = best_dist;
            });

        
        // calculate distances using optimized code
        kmean.update_cluster_assignments(&mut state, None);

        for i in 0..should_assignments.len() {
            assert_approx_eq!(state.centroid_distances[i], should_centroid_distances[i], max_diff);
        }
        assert_eq!(state.assignments, should_assignments);
    }

    #[bench]
    fn distance_matrix_calculation_benchmark_f64(b: &mut Bencher) { distance_matrix_calculation_benchmark::<f64>(b); }
    #[bench]
    fn distance_matrix_calculation_benchmark_f32(b: &mut Bencher) { distance_matrix_calculation_benchmark::<f32>(b); }

    fn distance_matrix_calculation_benchmark<T: Primitive>(b: &mut Bencher) where [T;LANES] : SimdArray, Simd<[T;LANES]>: SimdWrapper<T> {
        let sample_cnt = 20000;
        let sample_dims = 2000;
        let k = 8;

        let mut samples = vec![T::zero();sample_cnt * sample_dims];
        samples.iter_mut().for_each(|v| *v = thread_rng().gen_range(T::zero(), T::one()));
        let kmean = KMeans::new(samples, sample_cnt, sample_dims);

        let mut state = KMeansState::new(kmean.sample_cnt, kmean.p_sample_dims, k, KMeansEvt::empty());
        state.centroids.iter_mut()
            .zip(kmean.p_samples.iter())
            .for_each(|(c,s)| *c = *s);

        b.iter(|| {
            KMeans::update_cluster_assignments(&kmean, &mut state, None);
            state.clone()
        });
    }
}