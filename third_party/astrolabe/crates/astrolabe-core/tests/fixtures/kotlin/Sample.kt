package com.example.sample

import com.example.sample.util.Helper
import com.example.sample.model.*
import kotlinx.coroutines.launch as go

typealias Callback = (Int) -> Unit

const val MAX = 10
val (first, second) = Pair(1, 2)
var Int.doubled: Int
    get() = this * 2
    set(value) {}

fun <T> T.describe(x: Int): String = x.toString()

private fun hidden() {}

class Outer(val id: Int, plain: Int, private var label: String) : Base() {
    constructor(label: String) : this(0, 0, label)

    init {
        println(label)
    }

    val lazyValue by lazy { 1 }
    private val (a, b) = 1 to 2

    fun work() {
        val local = 1
        fun innerHelper() {}
        helper.run()
        Helper()
    }

    companion object {
        const val KEY = "k"
        fun create(): Outer = Outer(1, 2, "x")
    }

    object Registry {
        val entries = mutableListOf<String>()
    }

    inner class Inner {
        operator fun plus(other: Inner): Inner = this
    }

    val anon = object : Runnable {
        override fun run() {}
    }
}

class Named {
    companion object Factory
}

interface Shape {
    val area: Double
    fun draw()
}

fun interface Action {
    fun invoke()
}

enum class Color(val rgb: Int) {
    RED(0xff0000),
    GREEN(0x00ff00) {
        override fun label() = "g"
    };

    open fun label() = name
}

data class Point(val x: Int, val y: Int)

annotation class Marker

sealed class Result {
    class Ok(val value: Int) : Result()
    object Fail : Result()
}

abstract class Base
