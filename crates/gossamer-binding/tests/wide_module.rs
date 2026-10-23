//! A module of 300 items registers: the macros match a module's items in
//! one step, so their number is not bounded by the macro recursion limit.

gossamer_binding::register_module!(
    name: wide,
    doc: "Many items.",

    fn item_0(a: i64, b: i64, c: i64) -> i64 { a + b + c }

    fn item_1(a: i64, b: i64, c: i64) -> i64 { a + b + c + 1 }

    fn item_2(a: i64, b: i64, c: i64) -> i64 { a + b + c + 2 }

    fn item_3(a: i64, b: i64, c: i64) -> i64 { a + b + c + 3 }

    fn item_4(a: i64, b: i64, c: i64) -> i64 { a + b + c + 4 }

    fn item_5(a: i64, b: i64, c: i64) -> i64 { a + b + c + 5 }

    fn item_6(a: i64, b: i64, c: i64) -> i64 { a + b + c + 6 }

    fn item_7(a: i64, b: i64, c: i64) -> i64 { a + b + c + 7 }

    fn item_8(a: i64, b: i64, c: i64) -> i64 { a + b + c + 8 }

    fn item_9(a: i64, b: i64, c: i64) -> i64 { a + b + c + 9 }

    fn item_10(a: i64, b: i64, c: i64) -> i64 { a + b + c + 10 }

    fn item_11(a: i64, b: i64, c: i64) -> i64 { a + b + c + 11 }

    fn item_12(a: i64, b: i64, c: i64) -> i64 { a + b + c + 12 }

    fn item_13(a: i64, b: i64, c: i64) -> i64 { a + b + c + 13 }

    fn item_14(a: i64, b: i64, c: i64) -> i64 { a + b + c + 14 }

    fn item_15(a: i64, b: i64, c: i64) -> i64 { a + b + c + 15 }

    fn item_16(a: i64, b: i64, c: i64) -> i64 { a + b + c + 16 }

    fn item_17(a: i64, b: i64, c: i64) -> i64 { a + b + c + 17 }

    fn item_18(a: i64, b: i64, c: i64) -> i64 { a + b + c + 18 }

    fn item_19(a: i64, b: i64, c: i64) -> i64 { a + b + c + 19 }

    fn item_20(a: i64, b: i64, c: i64) -> i64 { a + b + c + 20 }

    fn item_21(a: i64, b: i64, c: i64) -> i64 { a + b + c + 21 }

    fn item_22(a: i64, b: i64, c: i64) -> i64 { a + b + c + 22 }

    fn item_23(a: i64, b: i64, c: i64) -> i64 { a + b + c + 23 }

    fn item_24(a: i64, b: i64, c: i64) -> i64 { a + b + c + 24 }

    fn item_25(a: i64, b: i64, c: i64) -> i64 { a + b + c + 25 }

    fn item_26(a: i64, b: i64, c: i64) -> i64 { a + b + c + 26 }

    fn item_27(a: i64, b: i64, c: i64) -> i64 { a + b + c + 27 }

    fn item_28(a: i64, b: i64, c: i64) -> i64 { a + b + c + 28 }

    fn item_29(a: i64, b: i64, c: i64) -> i64 { a + b + c + 29 }

    fn item_30(a: i64, b: i64, c: i64) -> i64 { a + b + c + 30 }

    fn item_31(a: i64, b: i64, c: i64) -> i64 { a + b + c + 31 }

    fn item_32(a: i64, b: i64, c: i64) -> i64 { a + b + c + 32 }

    fn item_33(a: i64, b: i64, c: i64) -> i64 { a + b + c + 33 }

    fn item_34(a: i64, b: i64, c: i64) -> i64 { a + b + c + 34 }

    fn item_35(a: i64, b: i64, c: i64) -> i64 { a + b + c + 35 }

    fn item_36(a: i64, b: i64, c: i64) -> i64 { a + b + c + 36 }

    fn item_37(a: i64, b: i64, c: i64) -> i64 { a + b + c + 37 }

    fn item_38(a: i64, b: i64, c: i64) -> i64 { a + b + c + 38 }

    fn item_39(a: i64, b: i64, c: i64) -> i64 { a + b + c + 39 }

    fn item_40(a: i64, b: i64, c: i64) -> i64 { a + b + c + 40 }

    fn item_41(a: i64, b: i64, c: i64) -> i64 { a + b + c + 41 }

    fn item_42(a: i64, b: i64, c: i64) -> i64 { a + b + c + 42 }

    fn item_43(a: i64, b: i64, c: i64) -> i64 { a + b + c + 43 }

    fn item_44(a: i64, b: i64, c: i64) -> i64 { a + b + c + 44 }

    fn item_45(a: i64, b: i64, c: i64) -> i64 { a + b + c + 45 }

    fn item_46(a: i64, b: i64, c: i64) -> i64 { a + b + c + 46 }

    fn item_47(a: i64, b: i64, c: i64) -> i64 { a + b + c + 47 }

    fn item_48(a: i64, b: i64, c: i64) -> i64 { a + b + c + 48 }

    fn item_49(a: i64, b: i64, c: i64) -> i64 { a + b + c + 49 }

    fn item_50(a: i64, b: i64, c: i64) -> i64 { a + b + c + 50 }

    fn item_51(a: i64, b: i64, c: i64) -> i64 { a + b + c + 51 }

    fn item_52(a: i64, b: i64, c: i64) -> i64 { a + b + c + 52 }

    fn item_53(a: i64, b: i64, c: i64) -> i64 { a + b + c + 53 }

    fn item_54(a: i64, b: i64, c: i64) -> i64 { a + b + c + 54 }

    fn item_55(a: i64, b: i64, c: i64) -> i64 { a + b + c + 55 }

    fn item_56(a: i64, b: i64, c: i64) -> i64 { a + b + c + 56 }

    fn item_57(a: i64, b: i64, c: i64) -> i64 { a + b + c + 57 }

    fn item_58(a: i64, b: i64, c: i64) -> i64 { a + b + c + 58 }

    fn item_59(a: i64, b: i64, c: i64) -> i64 { a + b + c + 59 }

    fn item_60(a: i64, b: i64, c: i64) -> i64 { a + b + c + 60 }

    fn item_61(a: i64, b: i64, c: i64) -> i64 { a + b + c + 61 }

    fn item_62(a: i64, b: i64, c: i64) -> i64 { a + b + c + 62 }

    fn item_63(a: i64, b: i64, c: i64) -> i64 { a + b + c + 63 }

    fn item_64(a: i64, b: i64, c: i64) -> i64 { a + b + c + 64 }

    fn item_65(a: i64, b: i64, c: i64) -> i64 { a + b + c + 65 }

    fn item_66(a: i64, b: i64, c: i64) -> i64 { a + b + c + 66 }

    fn item_67(a: i64, b: i64, c: i64) -> i64 { a + b + c + 67 }

    fn item_68(a: i64, b: i64, c: i64) -> i64 { a + b + c + 68 }

    fn item_69(a: i64, b: i64, c: i64) -> i64 { a + b + c + 69 }

    fn item_70(a: i64, b: i64, c: i64) -> i64 { a + b + c + 70 }

    fn item_71(a: i64, b: i64, c: i64) -> i64 { a + b + c + 71 }

    fn item_72(a: i64, b: i64, c: i64) -> i64 { a + b + c + 72 }

    fn item_73(a: i64, b: i64, c: i64) -> i64 { a + b + c + 73 }

    fn item_74(a: i64, b: i64, c: i64) -> i64 { a + b + c + 74 }

    fn item_75(a: i64, b: i64, c: i64) -> i64 { a + b + c + 75 }

    fn item_76(a: i64, b: i64, c: i64) -> i64 { a + b + c + 76 }

    fn item_77(a: i64, b: i64, c: i64) -> i64 { a + b + c + 77 }

    fn item_78(a: i64, b: i64, c: i64) -> i64 { a + b + c + 78 }

    fn item_79(a: i64, b: i64, c: i64) -> i64 { a + b + c + 79 }

    fn item_80(a: i64, b: i64, c: i64) -> i64 { a + b + c + 80 }

    fn item_81(a: i64, b: i64, c: i64) -> i64 { a + b + c + 81 }

    fn item_82(a: i64, b: i64, c: i64) -> i64 { a + b + c + 82 }

    fn item_83(a: i64, b: i64, c: i64) -> i64 { a + b + c + 83 }

    fn item_84(a: i64, b: i64, c: i64) -> i64 { a + b + c + 84 }

    fn item_85(a: i64, b: i64, c: i64) -> i64 { a + b + c + 85 }

    fn item_86(a: i64, b: i64, c: i64) -> i64 { a + b + c + 86 }

    fn item_87(a: i64, b: i64, c: i64) -> i64 { a + b + c + 87 }

    fn item_88(a: i64, b: i64, c: i64) -> i64 { a + b + c + 88 }

    fn item_89(a: i64, b: i64, c: i64) -> i64 { a + b + c + 89 }

    fn item_90(a: i64, b: i64, c: i64) -> i64 { a + b + c + 90 }

    fn item_91(a: i64, b: i64, c: i64) -> i64 { a + b + c + 91 }

    fn item_92(a: i64, b: i64, c: i64) -> i64 { a + b + c + 92 }

    fn item_93(a: i64, b: i64, c: i64) -> i64 { a + b + c + 93 }

    fn item_94(a: i64, b: i64, c: i64) -> i64 { a + b + c + 94 }

    fn item_95(a: i64, b: i64, c: i64) -> i64 { a + b + c + 95 }

    fn item_96(a: i64, b: i64, c: i64) -> i64 { a + b + c + 96 }

    fn item_97(a: i64, b: i64, c: i64) -> i64 { a + b + c + 97 }

    fn item_98(a: i64, b: i64, c: i64) -> i64 { a + b + c + 98 }

    fn item_99(a: i64, b: i64, c: i64) -> i64 { a + b + c + 99 }

    fn item_100(a: i64, b: i64, c: i64) -> i64 { a + b + c + 100 }

    fn item_101(a: i64, b: i64, c: i64) -> i64 { a + b + c + 101 }

    fn item_102(a: i64, b: i64, c: i64) -> i64 { a + b + c + 102 }

    fn item_103(a: i64, b: i64, c: i64) -> i64 { a + b + c + 103 }

    fn item_104(a: i64, b: i64, c: i64) -> i64 { a + b + c + 104 }

    fn item_105(a: i64, b: i64, c: i64) -> i64 { a + b + c + 105 }

    fn item_106(a: i64, b: i64, c: i64) -> i64 { a + b + c + 106 }

    fn item_107(a: i64, b: i64, c: i64) -> i64 { a + b + c + 107 }

    fn item_108(a: i64, b: i64, c: i64) -> i64 { a + b + c + 108 }

    fn item_109(a: i64, b: i64, c: i64) -> i64 { a + b + c + 109 }

    fn item_110(a: i64, b: i64, c: i64) -> i64 { a + b + c + 110 }

    fn item_111(a: i64, b: i64, c: i64) -> i64 { a + b + c + 111 }

    fn item_112(a: i64, b: i64, c: i64) -> i64 { a + b + c + 112 }

    fn item_113(a: i64, b: i64, c: i64) -> i64 { a + b + c + 113 }

    fn item_114(a: i64, b: i64, c: i64) -> i64 { a + b + c + 114 }

    fn item_115(a: i64, b: i64, c: i64) -> i64 { a + b + c + 115 }

    fn item_116(a: i64, b: i64, c: i64) -> i64 { a + b + c + 116 }

    fn item_117(a: i64, b: i64, c: i64) -> i64 { a + b + c + 117 }

    fn item_118(a: i64, b: i64, c: i64) -> i64 { a + b + c + 118 }

    fn item_119(a: i64, b: i64, c: i64) -> i64 { a + b + c + 119 }

    fn item_120(a: i64, b: i64, c: i64) -> i64 { a + b + c + 120 }

    fn item_121(a: i64, b: i64, c: i64) -> i64 { a + b + c + 121 }

    fn item_122(a: i64, b: i64, c: i64) -> i64 { a + b + c + 122 }

    fn item_123(a: i64, b: i64, c: i64) -> i64 { a + b + c + 123 }

    fn item_124(a: i64, b: i64, c: i64) -> i64 { a + b + c + 124 }

    fn item_125(a: i64, b: i64, c: i64) -> i64 { a + b + c + 125 }

    fn item_126(a: i64, b: i64, c: i64) -> i64 { a + b + c + 126 }

    fn item_127(a: i64, b: i64, c: i64) -> i64 { a + b + c + 127 }

    fn item_128(a: i64, b: i64, c: i64) -> i64 { a + b + c + 128 }

    fn item_129(a: i64, b: i64, c: i64) -> i64 { a + b + c + 129 }

    fn item_130(a: i64, b: i64, c: i64) -> i64 { a + b + c + 130 }

    fn item_131(a: i64, b: i64, c: i64) -> i64 { a + b + c + 131 }

    fn item_132(a: i64, b: i64, c: i64) -> i64 { a + b + c + 132 }

    fn item_133(a: i64, b: i64, c: i64) -> i64 { a + b + c + 133 }

    fn item_134(a: i64, b: i64, c: i64) -> i64 { a + b + c + 134 }

    fn item_135(a: i64, b: i64, c: i64) -> i64 { a + b + c + 135 }

    fn item_136(a: i64, b: i64, c: i64) -> i64 { a + b + c + 136 }

    fn item_137(a: i64, b: i64, c: i64) -> i64 { a + b + c + 137 }

    fn item_138(a: i64, b: i64, c: i64) -> i64 { a + b + c + 138 }

    fn item_139(a: i64, b: i64, c: i64) -> i64 { a + b + c + 139 }

    fn item_140(a: i64, b: i64, c: i64) -> i64 { a + b + c + 140 }

    fn item_141(a: i64, b: i64, c: i64) -> i64 { a + b + c + 141 }

    fn item_142(a: i64, b: i64, c: i64) -> i64 { a + b + c + 142 }

    fn item_143(a: i64, b: i64, c: i64) -> i64 { a + b + c + 143 }

    fn item_144(a: i64, b: i64, c: i64) -> i64 { a + b + c + 144 }

    fn item_145(a: i64, b: i64, c: i64) -> i64 { a + b + c + 145 }

    fn item_146(a: i64, b: i64, c: i64) -> i64 { a + b + c + 146 }

    fn item_147(a: i64, b: i64, c: i64) -> i64 { a + b + c + 147 }

    fn item_148(a: i64, b: i64, c: i64) -> i64 { a + b + c + 148 }

    fn item_149(a: i64, b: i64, c: i64) -> i64 { a + b + c + 149 }

    fn item_150(a: i64, b: i64, c: i64) -> i64 { a + b + c + 150 }

    fn item_151(a: i64, b: i64, c: i64) -> i64 { a + b + c + 151 }

    fn item_152(a: i64, b: i64, c: i64) -> i64 { a + b + c + 152 }

    fn item_153(a: i64, b: i64, c: i64) -> i64 { a + b + c + 153 }

    fn item_154(a: i64, b: i64, c: i64) -> i64 { a + b + c + 154 }

    fn item_155(a: i64, b: i64, c: i64) -> i64 { a + b + c + 155 }

    fn item_156(a: i64, b: i64, c: i64) -> i64 { a + b + c + 156 }

    fn item_157(a: i64, b: i64, c: i64) -> i64 { a + b + c + 157 }

    fn item_158(a: i64, b: i64, c: i64) -> i64 { a + b + c + 158 }

    fn item_159(a: i64, b: i64, c: i64) -> i64 { a + b + c + 159 }

    fn item_160(a: i64, b: i64, c: i64) -> i64 { a + b + c + 160 }

    fn item_161(a: i64, b: i64, c: i64) -> i64 { a + b + c + 161 }

    fn item_162(a: i64, b: i64, c: i64) -> i64 { a + b + c + 162 }

    fn item_163(a: i64, b: i64, c: i64) -> i64 { a + b + c + 163 }

    fn item_164(a: i64, b: i64, c: i64) -> i64 { a + b + c + 164 }

    fn item_165(a: i64, b: i64, c: i64) -> i64 { a + b + c + 165 }

    fn item_166(a: i64, b: i64, c: i64) -> i64 { a + b + c + 166 }

    fn item_167(a: i64, b: i64, c: i64) -> i64 { a + b + c + 167 }

    fn item_168(a: i64, b: i64, c: i64) -> i64 { a + b + c + 168 }

    fn item_169(a: i64, b: i64, c: i64) -> i64 { a + b + c + 169 }

    fn item_170(a: i64, b: i64, c: i64) -> i64 { a + b + c + 170 }

    fn item_171(a: i64, b: i64, c: i64) -> i64 { a + b + c + 171 }

    fn item_172(a: i64, b: i64, c: i64) -> i64 { a + b + c + 172 }

    fn item_173(a: i64, b: i64, c: i64) -> i64 { a + b + c + 173 }

    fn item_174(a: i64, b: i64, c: i64) -> i64 { a + b + c + 174 }

    fn item_175(a: i64, b: i64, c: i64) -> i64 { a + b + c + 175 }

    fn item_176(a: i64, b: i64, c: i64) -> i64 { a + b + c + 176 }

    fn item_177(a: i64, b: i64, c: i64) -> i64 { a + b + c + 177 }

    fn item_178(a: i64, b: i64, c: i64) -> i64 { a + b + c + 178 }

    fn item_179(a: i64, b: i64, c: i64) -> i64 { a + b + c + 179 }

    fn item_180(a: i64, b: i64, c: i64) -> i64 { a + b + c + 180 }

    fn item_181(a: i64, b: i64, c: i64) -> i64 { a + b + c + 181 }

    fn item_182(a: i64, b: i64, c: i64) -> i64 { a + b + c + 182 }

    fn item_183(a: i64, b: i64, c: i64) -> i64 { a + b + c + 183 }

    fn item_184(a: i64, b: i64, c: i64) -> i64 { a + b + c + 184 }

    fn item_185(a: i64, b: i64, c: i64) -> i64 { a + b + c + 185 }

    fn item_186(a: i64, b: i64, c: i64) -> i64 { a + b + c + 186 }

    fn item_187(a: i64, b: i64, c: i64) -> i64 { a + b + c + 187 }

    fn item_188(a: i64, b: i64, c: i64) -> i64 { a + b + c + 188 }

    fn item_189(a: i64, b: i64, c: i64) -> i64 { a + b + c + 189 }

    fn item_190(a: i64, b: i64, c: i64) -> i64 { a + b + c + 190 }

    fn item_191(a: i64, b: i64, c: i64) -> i64 { a + b + c + 191 }

    fn item_192(a: i64, b: i64, c: i64) -> i64 { a + b + c + 192 }

    fn item_193(a: i64, b: i64, c: i64) -> i64 { a + b + c + 193 }

    fn item_194(a: i64, b: i64, c: i64) -> i64 { a + b + c + 194 }

    fn item_195(a: i64, b: i64, c: i64) -> i64 { a + b + c + 195 }

    fn item_196(a: i64, b: i64, c: i64) -> i64 { a + b + c + 196 }

    fn item_197(a: i64, b: i64, c: i64) -> i64 { a + b + c + 197 }

    fn item_198(a: i64, b: i64, c: i64) -> i64 { a + b + c + 198 }

    fn item_199(a: i64, b: i64, c: i64) -> i64 { a + b + c + 199 }

    fn item_200(a: i64, b: i64, c: i64) -> i64 { a + b + c + 200 }

    fn item_201(a: i64, b: i64, c: i64) -> i64 { a + b + c + 201 }

    fn item_202(a: i64, b: i64, c: i64) -> i64 { a + b + c + 202 }

    fn item_203(a: i64, b: i64, c: i64) -> i64 { a + b + c + 203 }

    fn item_204(a: i64, b: i64, c: i64) -> i64 { a + b + c + 204 }

    fn item_205(a: i64, b: i64, c: i64) -> i64 { a + b + c + 205 }

    fn item_206(a: i64, b: i64, c: i64) -> i64 { a + b + c + 206 }

    fn item_207(a: i64, b: i64, c: i64) -> i64 { a + b + c + 207 }

    fn item_208(a: i64, b: i64, c: i64) -> i64 { a + b + c + 208 }

    fn item_209(a: i64, b: i64, c: i64) -> i64 { a + b + c + 209 }

    fn item_210(a: i64, b: i64, c: i64) -> i64 { a + b + c + 210 }

    fn item_211(a: i64, b: i64, c: i64) -> i64 { a + b + c + 211 }

    fn item_212(a: i64, b: i64, c: i64) -> i64 { a + b + c + 212 }

    fn item_213(a: i64, b: i64, c: i64) -> i64 { a + b + c + 213 }

    fn item_214(a: i64, b: i64, c: i64) -> i64 { a + b + c + 214 }

    fn item_215(a: i64, b: i64, c: i64) -> i64 { a + b + c + 215 }

    fn item_216(a: i64, b: i64, c: i64) -> i64 { a + b + c + 216 }

    fn item_217(a: i64, b: i64, c: i64) -> i64 { a + b + c + 217 }

    fn item_218(a: i64, b: i64, c: i64) -> i64 { a + b + c + 218 }

    fn item_219(a: i64, b: i64, c: i64) -> i64 { a + b + c + 219 }

    fn item_220(a: i64, b: i64, c: i64) -> i64 { a + b + c + 220 }

    fn item_221(a: i64, b: i64, c: i64) -> i64 { a + b + c + 221 }

    fn item_222(a: i64, b: i64, c: i64) -> i64 { a + b + c + 222 }

    fn item_223(a: i64, b: i64, c: i64) -> i64 { a + b + c + 223 }

    fn item_224(a: i64, b: i64, c: i64) -> i64 { a + b + c + 224 }

    fn item_225(a: i64, b: i64, c: i64) -> i64 { a + b + c + 225 }

    fn item_226(a: i64, b: i64, c: i64) -> i64 { a + b + c + 226 }

    fn item_227(a: i64, b: i64, c: i64) -> i64 { a + b + c + 227 }

    fn item_228(a: i64, b: i64, c: i64) -> i64 { a + b + c + 228 }

    fn item_229(a: i64, b: i64, c: i64) -> i64 { a + b + c + 229 }

    fn item_230(a: i64, b: i64, c: i64) -> i64 { a + b + c + 230 }

    fn item_231(a: i64, b: i64, c: i64) -> i64 { a + b + c + 231 }

    fn item_232(a: i64, b: i64, c: i64) -> i64 { a + b + c + 232 }

    fn item_233(a: i64, b: i64, c: i64) -> i64 { a + b + c + 233 }

    fn item_234(a: i64, b: i64, c: i64) -> i64 { a + b + c + 234 }

    fn item_235(a: i64, b: i64, c: i64) -> i64 { a + b + c + 235 }

    fn item_236(a: i64, b: i64, c: i64) -> i64 { a + b + c + 236 }

    fn item_237(a: i64, b: i64, c: i64) -> i64 { a + b + c + 237 }

    fn item_238(a: i64, b: i64, c: i64) -> i64 { a + b + c + 238 }

    fn item_239(a: i64, b: i64, c: i64) -> i64 { a + b + c + 239 }

    fn item_240(a: i64, b: i64, c: i64) -> i64 { a + b + c + 240 }

    fn item_241(a: i64, b: i64, c: i64) -> i64 { a + b + c + 241 }

    fn item_242(a: i64, b: i64, c: i64) -> i64 { a + b + c + 242 }

    fn item_243(a: i64, b: i64, c: i64) -> i64 { a + b + c + 243 }

    fn item_244(a: i64, b: i64, c: i64) -> i64 { a + b + c + 244 }

    fn item_245(a: i64, b: i64, c: i64) -> i64 { a + b + c + 245 }

    fn item_246(a: i64, b: i64, c: i64) -> i64 { a + b + c + 246 }

    fn item_247(a: i64, b: i64, c: i64) -> i64 { a + b + c + 247 }

    fn item_248(a: i64, b: i64, c: i64) -> i64 { a + b + c + 248 }

    fn item_249(a: i64, b: i64, c: i64) -> i64 { a + b + c + 249 }

    fn item_250(a: i64, b: i64, c: i64) -> i64 { a + b + c + 250 }

    fn item_251(a: i64, b: i64, c: i64) -> i64 { a + b + c + 251 }

    fn item_252(a: i64, b: i64, c: i64) -> i64 { a + b + c + 252 }

    fn item_253(a: i64, b: i64, c: i64) -> i64 { a + b + c + 253 }

    fn item_254(a: i64, b: i64, c: i64) -> i64 { a + b + c + 254 }

    fn item_255(a: i64, b: i64, c: i64) -> i64 { a + b + c + 255 }

    fn item_256(a: i64, b: i64, c: i64) -> i64 { a + b + c + 256 }

    fn item_257(a: i64, b: i64, c: i64) -> i64 { a + b + c + 257 }

    fn item_258(a: i64, b: i64, c: i64) -> i64 { a + b + c + 258 }

    fn item_259(a: i64, b: i64, c: i64) -> i64 { a + b + c + 259 }

    fn item_260(a: i64, b: i64, c: i64) -> i64 { a + b + c + 260 }

    fn item_261(a: i64, b: i64, c: i64) -> i64 { a + b + c + 261 }

    fn item_262(a: i64, b: i64, c: i64) -> i64 { a + b + c + 262 }

    fn item_263(a: i64, b: i64, c: i64) -> i64 { a + b + c + 263 }

    fn item_264(a: i64, b: i64, c: i64) -> i64 { a + b + c + 264 }

    fn item_265(a: i64, b: i64, c: i64) -> i64 { a + b + c + 265 }

    fn item_266(a: i64, b: i64, c: i64) -> i64 { a + b + c + 266 }

    fn item_267(a: i64, b: i64, c: i64) -> i64 { a + b + c + 267 }

    fn item_268(a: i64, b: i64, c: i64) -> i64 { a + b + c + 268 }

    fn item_269(a: i64, b: i64, c: i64) -> i64 { a + b + c + 269 }

    fn item_270(a: i64, b: i64, c: i64) -> i64 { a + b + c + 270 }

    fn item_271(a: i64, b: i64, c: i64) -> i64 { a + b + c + 271 }

    fn item_272(a: i64, b: i64, c: i64) -> i64 { a + b + c + 272 }

    fn item_273(a: i64, b: i64, c: i64) -> i64 { a + b + c + 273 }

    fn item_274(a: i64, b: i64, c: i64) -> i64 { a + b + c + 274 }

    fn item_275(a: i64, b: i64, c: i64) -> i64 { a + b + c + 275 }

    fn item_276(a: i64, b: i64, c: i64) -> i64 { a + b + c + 276 }

    fn item_277(a: i64, b: i64, c: i64) -> i64 { a + b + c + 277 }

    fn item_278(a: i64, b: i64, c: i64) -> i64 { a + b + c + 278 }

    fn item_279(a: i64, b: i64, c: i64) -> i64 { a + b + c + 279 }

    fn item_280(a: i64, b: i64, c: i64) -> i64 { a + b + c + 280 }

    fn item_281(a: i64, b: i64, c: i64) -> i64 { a + b + c + 281 }

    fn item_282(a: i64, b: i64, c: i64) -> i64 { a + b + c + 282 }

    fn item_283(a: i64, b: i64, c: i64) -> i64 { a + b + c + 283 }

    fn item_284(a: i64, b: i64, c: i64) -> i64 { a + b + c + 284 }

    fn item_285(a: i64, b: i64, c: i64) -> i64 { a + b + c + 285 }

    fn item_286(a: i64, b: i64, c: i64) -> i64 { a + b + c + 286 }

    fn item_287(a: i64, b: i64, c: i64) -> i64 { a + b + c + 287 }

    fn item_288(a: i64, b: i64, c: i64) -> i64 { a + b + c + 288 }

    fn item_289(a: i64, b: i64, c: i64) -> i64 { a + b + c + 289 }

    fn item_290(a: i64, b: i64, c: i64) -> i64 { a + b + c + 290 }

    fn item_291(a: i64, b: i64, c: i64) -> i64 { a + b + c + 291 }

    fn item_292(a: i64, b: i64, c: i64) -> i64 { a + b + c + 292 }

    fn item_293(a: i64, b: i64, c: i64) -> i64 { a + b + c + 293 }

    fn item_294(a: i64, b: i64, c: i64) -> i64 { a + b + c + 294 }

    fn item_295(a: i64, b: i64, c: i64) -> i64 { a + b + c + 295 }

    fn item_296(a: i64, b: i64, c: i64) -> i64 { a + b + c + 296 }

    fn item_297(a: i64, b: i64, c: i64) -> i64 { a + b + c + 297 }

    fn item_298(a: i64, b: i64, c: i64) -> i64 { a + b + c + 298 }

    fn item_299(a: i64, b: i64, c: i64) -> i64 { a + b + c + 299 }
);

#[test]
fn a_module_of_three_hundred_items_registers_and_calls() {
    let module = gossamer_binding::module("wide").expect("wide registered");
    assert_eq!(module.items.len(), 300);
    let out = gossamer_binding::testing::call(
        "wide::item_299",
        &[
            gossamer_binding::Value::Int(1),
            gossamer_binding::Value::Int(2),
            gossamer_binding::Value::Int(3),
        ],
    )
    .unwrap();
    assert_eq!(out, gossamer_binding::Value::Int(305));
}
