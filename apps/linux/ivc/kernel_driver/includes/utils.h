#pragma once

#define INFO(args...)                                                          \
	do                                                                         \
	{                                                                          \
		pr_info("[AXINFO] " args);                                             \
	} while (0)

#define WARNING(args...)                                                       \
	do                                                                         \
	{                                                                          \
		pr_err("[AXWARNING] " args);                                           \
	} while (0)

#define ERROR(args...)                                                         \
	do                                                                         \
	{                                                                          \
		pr_err("[AXERROR] " args);                                             \
	} while (0)

#define MRS(var, reg) asm volatile("mrs %0, " #reg "\n\r" : "=r"(var))
#define MSR(reg, var) asm volatile("msr " #reg ", %0\n\r" ::"r"(var))

#define PAR_MASK (0x0000FFFFFFFFF000)
#define PAR_F (1ULL << 0)

static inline u64 kva2pa(u64 va)
{
	u64 par = 0, par_saved = 0;
	MRS(par_saved, PAR_EL1);
	asm volatile("AT S1E1W, %0" ::"r"(va));
	MRS(par, PAR_EL1);
	MSR(PAR_EL1, par_saved);
	if (par & PAR_F)
		return ~0ULL;

	return (par & PAR_MASK) | (((uint64_t)va) & (0x1000 - 1));
}
