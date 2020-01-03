#!/usr/bin/ruby

def read_bytecodes
    bytecodes = []
    parse_bytecode = false

    File.open('dora/src/bytecode/data.rs').each_line do |line|
        unless parse_bytecode
            parse_bytecode = true if line == "pub enum BytecodeOpcode {\n"
            next
        end

        next if line.strip.empty?
        next if line.match(/^\s*\/\//)

        return bytecodes if line == "}\n"

        m = line.match(/^\s*([a-zA-Z]+),$/)

        unless m
            raise "illegal bytecode: #{line.inspect}"
        end

        name = m[1]
        bytecodes.push(name)
    end
end

def write_bytecodes(bytecodes)
    File.open('dora-boots/bytecode_opcode.dora', 'w') do |f|
        f.puts "// generated by tools/bytecode-gen.rb"
        f.puts

        opcode = 0

        for bytecode in bytecodes
            f.puts "const #{dora_name(bytecode)}: Int = #{opcode};"
            opcode += 1
        end

        f.puts
        f.puts "fun bytecodeName(opcode: Int) -> String {"

        for bytecode in bytecodes
            f.puts "  if opcode == #{dora_name(bytecode)} { return #{bytecode.inspect}; }"
        end

        f.puts "  \"UNKNOWN(${opcode})\""

        f.puts "}"
    end
end

def dora_name(name)
    result = name.gsub(/(.)([A-Z])/, '\1_\2')
    "BC_#{result.upcase}"
end

bytecodes = read_bytecodes
write_bytecodes(bytecodes)