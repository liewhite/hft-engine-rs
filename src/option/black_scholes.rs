//! Black-Scholes 欧式期权定价与希腊字母 —— 纯函数，无副作用、无外部依赖，便于直接断言测试。
//! (移植自 ox-demo `hft.option.BlackScholes`)

/// 期权类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptionRight {
    Call,
    Put,
}

/// 单份期权的 Black-Scholes 希腊字母 (per-contract)。
///
/// 单位约定 (标准 BS, 纯数学定义)：
///   - delta: 无量纲, dPrice/dS, call∈[0,1] put∈[-1,0]
///   - gamma: d²Price/dS²
///   - vega : dPrice/dσ, 对 1.0 (=100%) 波动率
///   - theta: dPrice/dt, **每年** (负值表示时间衰减)
///   - price: 理论价 (与标的同计价单位)
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BsGreeks {
    pub price: f64,
    pub delta: f64,
    pub gamma: f64,
    pub vega: f64,
    pub theta: f64,
}

/// 一年的毫秒数 (365 日)，用于把到期时间差换算为年化剩余期限
pub const MILLIS_PER_YEAR: f64 = 365.0 * 24.0 * 60.0 * 60.0 * 1000.0;

/// 标准正态分布概率密度函数
pub fn norm_pdf(x: f64) -> f64 {
    (-0.5 * x * x).exp() / (2.0 * std::f64::consts::PI).sqrt()
}

/// 标准正态分布累积分布函数 N(x) = 0.5·erfc(-x/√2)，erfc 用 Numerical Recipes 有理逼近 (|误差|<1.2e-7)
pub fn norm_cdf(x: f64) -> f64 {
    0.5 * erfc(-x / std::f64::consts::SQRT_2)
}

fn erfc(x: f64) -> f64 {
    let z = x.abs();
    let t = 1.0 / (1.0 + 0.5 * z);
    let ans = t
        * (-z * z - 1.26551223
            + t * (1.00002368
                + t * (0.37409196
                    + t * (0.09678418
                        + t * (-0.18628806
                            + t * (0.27886807
                                + t * (-1.13520398
                                    + t * (1.48851587
                                        + t * (-0.82215223 + t * 0.17087277)))))))))
        .exp();
    if x >= 0.0 {
        ans
    } else {
        2.0 - ans
    }
}

/// 计算单份期权的希腊字母。
///
/// - `s`      标的价 (>0)
/// - `k`      行权价 (>0)
/// - `t_years` 年化剩余期限；<=0 视为已到期，返回内在价值与零阶以上希腊字母
/// - `sigma`  年化隐含波动率 (>0)
/// - `r`      无风险年化利率
pub fn greeks(right: OptionRight, s: f64, k: f64, t_years: f64, sigma: f64, r: f64) -> BsGreeks {
    // 边界：到期 / 非法波动率或价格 -> 退化为内在价值，导数为 0 (避免除零)
    if t_years <= 0.0 || sigma <= 0.0 || s <= 0.0 || k <= 0.0 {
        let intrinsic = match right {
            OptionRight::Call => (s - k).max(0.0),
            OptionRight::Put => (k - s).max(0.0),
        };
        let delta = match right {
            OptionRight::Call => {
                if s > k {
                    1.0
                } else {
                    0.0
                }
            }
            OptionRight::Put => {
                if s < k {
                    -1.0
                } else {
                    0.0
                }
            }
        };
        return BsGreeks {
            price: intrinsic,
            delta,
            gamma: 0.0,
            vega: 0.0,
            theta: 0.0,
        };
    }

    let sqrt_t = t_years.sqrt();
    let d1 = ((s / k).ln() + (r + 0.5 * sigma * sigma) * t_years) / (sigma * sqrt_t);
    let d2 = d1 - sigma * sqrt_t;
    let nd1 = norm_pdf(d1);
    let discount = (-r * t_years).exp();
    let gamma = nd1 / (s * sigma * sqrt_t);
    let vega = s * nd1 * sqrt_t;
    match right {
        OptionRight::Call => {
            let delta = norm_cdf(d1);
            let price = s * delta - k * discount * norm_cdf(d2);
            let theta = -(s * nd1 * sigma) / (2.0 * sqrt_t) - r * k * discount * norm_cdf(d2);
            BsGreeks {
                price,
                delta,
                gamma,
                vega,
                theta,
            }
        }
        OptionRight::Put => {
            let delta = norm_cdf(d1) - 1.0;
            let price = k * discount * norm_cdf(-d2) - s * norm_cdf(-d1);
            let theta = -(s * nd1 * sigma) / (2.0 * sqrt_t) + r * k * discount * norm_cdf(-d2);
            BsGreeks {
                price,
                delta,
                gamma,
                vega,
                theta,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::OptionRight::*;
    use super::*;

    fn near(a: f64, b: f64, eps: f64) {
        assert!((a - b).abs() < eps, "expected {b}, got {a} (eps={eps})");
    }

    #[test]
    fn norm_cdf_key_points() {
        near(norm_cdf(0.0), 0.5, 1e-3);
        near(norm_cdf(1.96), 0.975, 1e-3);
        near(norm_cdf(-1.96), 0.025, 1e-3);
    }

    #[test]
    fn atm_call_reference() {
        // S=K=100, T=1, σ=0.2, r=0
        let g = greeks(Call, 100.0, 100.0, 1.0, 0.2, 0.0);
        near(g.delta, 0.53983, 1e-4);
        near(g.price, 7.9656, 1e-3);
        near(g.gamma, 0.019848, 1e-5);
        near(g.vega, 39.695, 1e-2);
        assert!(g.theta < 0.0, "long option theta 应为负");
    }

    #[test]
    fn put_call_parity() {
        let (s, k, t, sigma, r) = (100.0, 95.0, 0.75, 0.3, 0.05);
        let c = greeks(Call, s, k, t, sigma, r);
        let p = greeks(Put, s, k, t, sigma, r);
        near(c.price - p.price, s - k * (-r * t).exp(), 1e-6);
        near(c.delta - p.delta, 1.0, 1e-9);
        near(c.gamma, p.gamma, 1e-12);
        near(c.vega, p.vega, 1e-9);
    }

    #[test]
    fn delta_ranges() {
        let c = greeks(Call, 120.0, 100.0, 0.5, 0.4, 0.0);
        let p = greeks(Put, 80.0, 100.0, 0.5, 0.4, 0.0);
        assert!(c.delta > 0.0 && c.delta < 1.0, "call delta={}", c.delta);
        assert!(p.delta > -1.0 && p.delta < 0.0, "put delta={}", p.delta);
    }

    #[test]
    fn expiry_boundary_intrinsic() {
        let itm_call = greeks(Call, 110.0, 100.0, 0.0, 0.2, 0.0);
        near(itm_call.price, 10.0, 1e-3);
        near(itm_call.delta, 1.0, 1e-3);
        near(itm_call.gamma, 0.0, 1e-3);
        near(itm_call.vega, 0.0, 1e-3);
        near(itm_call.theta, 0.0, 1e-3);
        let otm_put = greeks(Put, 110.0, 100.0, -0.1, 0.2, 0.0);
        near(otm_put.price, 0.0, 1e-3);
        near(otm_put.delta, 0.0, 1e-3);
    }

    #[test]
    fn illegal_vol_degenerates() {
        let g = greeks(Call, 100.0, 100.0, 1.0, 0.0, 0.0);
        assert!(g.gamma == 0.0 && g.vega == 0.0);
    }
}
